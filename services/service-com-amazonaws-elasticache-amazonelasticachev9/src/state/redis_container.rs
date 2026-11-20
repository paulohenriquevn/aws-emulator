// SPDX-License-Identifier: CC0-1.0

//! fork: a Redis OSS or Valkey cache served by a real server in a container.
//!
//! With [`BACKEND_ENV`] set to `docker`, a replication group with cluster mode disabled and a
//! standalone cache cluster of engine `redis` or `valkey` start a container of that engine
//! (`redis:<major>` or `valkey/valkey:<major>`). The container listens on a loopback port
//! only. What the cache publishes as its endpoints (the group's primary and reader endpoints,
//! and every node's endpoint) is a listener of this emulator, and every connection it accepts
//! is passed on byte for byte to the container ([`RedisProxy`]).
//!
//! `AuthToken` becomes the server's `requirepass`, so a command sent before `AUTH` is refused
//! with the server's own `NOAUTH` error. With `TransitEncryptionEnabled`, the listener runs TLS
//! with the emulator's certificate from the first byte, as an ElastiCache endpoint does; the
//! container itself runs without TLS, on loopback.
//!
//! The container is removed when the cache's [`RedisEndpoint`] is dropped, which deleting the
//! group or the cluster does. Containers carry the label [`CONTAINER_LABEL`], so the ones an
//! emulator that was killed left behind can be found and removed.

use std::net::SocketAddr;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::watch;

/// The environment variable that chooses the backend of Redis OSS and Valkey caches.
pub const BACKEND_ENV: &str = "EMULATOR_ELASTICACHE_BACKEND";

/// The label every container this module starts carries.
pub const CONTAINER_LABEL: &str = "aws-emulator.service=elasticache";

/// How long a container may take to answer `PING`, image pull included.
const READY_TIMEOUT: Duration = Duration::from_secs(300);

/// How often readiness is probed while the container starts.
const READY_POLL: Duration = Duration::from_millis(500);

/// How long a connection waits for a container that is still starting before it is closed.
const SESSION_READY_WAIT: Duration = Duration::from_secs(30);

/// The port the server listens on inside the container.
const CONTAINER_PORT: &str = "6379/tcp";

/// Whether `value` of [`BACKEND_ENV`] turns the container backend on: `docker`, in any case,
/// surrounding whitespace ignored.
#[must_use]
pub fn docker_backend_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.trim().eq_ignore_ascii_case("docker"))
}

/// Whether the process is configured for the container backend.
#[must_use]
pub fn docker_backend_configured() -> bool {
    docker_backend_enabled(std::env::var(BACKEND_ENV).ok().as_deref())
}

/// The image and the server binary a cache of `engine` at `engine_version` runs, or `None`
/// for an engine this module does not serve (Memcached). The tag is the major version (the
/// part before the first dot), and a cache with no version runs the newest major the
/// engine's image publishes under that number here.
#[must_use]
pub fn image_for(engine: &str, engine_version: Option<&str>) -> Option<(String, &'static str)> {
    let major = engine_version
        .map(str::trim)
        .and_then(|version| version.split('.').next())
        .filter(|major| !major.is_empty());
    match engine.to_ascii_lowercase().as_str() {
        "valkey" => Some((format!("valkey/valkey:{}", major.unwrap_or("8")), "valkey-server")),
        "redis" => Some((format!("redis:{}", major.unwrap_or("7")), "redis-server")),
        _ => None,
    }
}

/// The container name for the cache `id` (a replication group or cache cluster id, which is
/// lowercase letters, digits and hyphens).
#[must_use]
pub fn container_name(id: &str) -> String {
    let safe: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    format!("aws-emulator-elasticache-{safe}")
}

/// What the container is started with.
#[derive(Clone)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    /// `valkey-server` or `redis-server`.
    pub server: &'static str,
    /// `AuthToken`, the server's `requirepass`.
    pub password: Option<String>,
}

impl std::fmt::Debug for ContainerSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerSpec")
            .field("name", &self.name)
            .field("image", &self.image)
            .field("server", &self.server)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Where a container has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ContainerState {
    Starting,
    Ready(SocketAddr),
    Failed(String),
}

/// A Redis OSS or Valkey container started for one cache. Dropping it removes the container.
pub struct RedisContainer {
    name: String,
    state: watch::Receiver<ContainerState>,
}

impl std::fmt::Debug for RedisContainer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisContainer")
            .field("name", &self.name)
            .field("state", &*self.state.borrow())
            .finish()
    }
}

impl RedisContainer {
    /// Start the container `spec` describes, in the background.
    #[must_use]
    pub fn start(spec: ContainerSpec) -> Arc<Self> {
        let (sender, receiver) = watch::channel(ContainerState::Starting);
        let name = spec.name.clone();
        tokio::spawn(async move {
            let outcome = run_container(&spec).await;
            if let Err(reason) = &outcome {
                tracing::error!(container = %spec.name, image = %spec.image, %reason, "the cache container did not start");
            }
            let _ = sender.send(match outcome {
                Ok(address) => ContainerState::Ready(address),
                Err(reason) => ContainerState::Failed(reason),
            });
        });
        Arc::new(Self { name, state: receiver })
    }

    /// The container's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The loopback address the server accepts connections on, once it does.
    pub async fn ready(&self) -> Result<SocketAddr, String> {
        let mut state = self.state.clone();
        loop {
            match &*state.borrow_and_update() {
                ContainerState::Ready(address) => return Ok(*address),
                ContainerState::Failed(reason) => return Err(reason.clone()),
                ContainerState::Starting => {}
            }
            if state.changed().await.is_err() {
                return Err("the container's start task ended without an outcome".to_string());
            }
        }
    }
}

impl Drop for RedisContainer {
    fn drop(&mut self) {
        let name = self.name.clone();
        // Removal must not block the request that dropped the endpoint.
        std::thread::spawn(move || {
            match std::process::Command::new("docker")
                .args(["rm", "-f", "-v", &name])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
            {
                Ok(output) if output.status.success() => {
                    tracing::debug!(container = %name, "removed the cache container");
                }
                Ok(output) => tracing::warn!(
                    container = %name,
                    stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                    "docker rm of the cache container failed"
                ),
                Err(error) => tracing::warn!(container = %name, %error, "docker rm could not be started"),
            }
        });
    }
}

/// `docker run` the container, then wait until the server answers `PING` on the published
/// port. Answers that loopback address.
async fn run_container(spec: &ContainerSpec) -> Result<SocketAddr, String> {
    // A container of the same name is one an earlier emulator left behind.
    let _ = Command::new("docker")
        .args(["rm", "-f", "-v", &spec.name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    let mut command = Command::new("docker");
    command.args(["run", "-d", "--name", &spec.name, "--label", CONTAINER_LABEL, "-p", "127.0.0.1::6379"]);
    match &spec.password {
        // The token comes from this process's environment and reaches the server through the
        // container's, so it is on no command line.
        Some(password) => {
            command
                .args(["-e", "CACHE_AUTH_TOKEN", &spec.image, "sh", "-c"])
                .arg(format!("exec {} --requirepass \"$CACHE_AUTH_TOKEN\"", spec.server))
                .env("CACHE_AUTH_TOKEN", password);
        }
        None => {
            command.arg(&spec.image);
        }
    }
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| format!("docker run could not be started: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "docker run {} exited with {}: {}",
            spec.image,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let address = published_address(&spec.name).await?;
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        if answers_ping(address).await {
            tracing::info!(container = %spec.name, image = %spec.image, %address, "the cache container accepts connections");
            return Ok(address);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("{} did not answer PING within {READY_TIMEOUT:?}", spec.name));
        }
        tokio::time::sleep(READY_POLL).await;
    }
}

/// Whether a server on `address` answers `PING`: `+PONG`, or the `-NOAUTH` error of a server
/// that requires a password, both of which only a running server sends.
async fn answers_ping(address: SocketAddr) -> bool {
    let attempt = async {
        let mut stream = tokio::net::TcpStream::connect(address).await.ok()?;
        stream.write_all(b"PING\r\n").await.ok()?;
        let mut reply = [0u8; 64];
        let read = stream.read(&mut reply).await.ok()?;
        Some(is_ping_answer(&reply[..read]))
    };
    tokio::time::timeout(Duration::from_secs(2), attempt).await.ok().flatten().unwrap_or(false)
}

/// Whether `reply` is what a running server answers `PING` with.
#[must_use]
pub fn is_ping_answer(reply: &[u8]) -> bool {
    reply.starts_with(b"+PONG") || reply.starts_with(b"-NOAUTH")
}

/// The loopback address Docker published the container's port on.
async fn published_address(name: &str) -> Result<SocketAddr, String> {
    let output = Command::new("docker")
        .args(["port", name, CONTAINER_PORT])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| format!("docker port could not be started: {error}"))?;
    let listed = String::from_utf8_lossy(&output.stdout);
    listed
        .lines()
        .filter_map(|line| line.trim().parse::<SocketAddr>().ok())
        .find(SocketAddr::is_ipv4)
        .ok_or_else(|| {
            format!(
                "docker port {name} {CONTAINER_PORT} named no IPv4 address: {:?} {}",
                listed.trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            )
        })
}

/// The handler of a container-backed cache's endpoint.
pub struct RedisProxy {
    container: Arc<RedisContainer>,
    /// The core's TLS server, for a cache with transit encryption.
    tls: Option<::emulator_server::TlsUpgrader>,
    /// The cache's security groups, applied to each connection by its source address.
    guard: Option<ConnectionGuard>,
}

type Ec2Store = ::engine::ServiceState<::service_com_amazonaws_ec2_amazonec2::state::ComAmazonawsEc2Amazonec2State>;

/// What decides whether a source may connect to a cache: the cache's security groups, read
/// from its record when the connection arrives, so a `Modify...` that changes them applies
/// to the next connection, and the account's EC2 store, which holds the groups' rules and
/// the network interface a source address belongs to.
#[derive(Clone)]
pub struct ConnectionGuard {
    state: ::engine::ServiceState<super::ComAmazonawsElasticacheAmazonelasticachev9State>,
    /// The replication group or cache cluster id.
    id: String,
    /// The cache's `Port`, the port the inbound rules are read for.
    port: i32,
    firewall: Ec2Store,
}

impl ConnectionGuard {
    /// Whether the cache's security groups refuse a connection from `peer`: only a verdict
    /// of the groups refuses, and an undecided question (a source no network interface of
    /// the account holds, a group EC2 does not hold) lets the connection through.
    fn refuses(&self, peer: Option<std::net::SocketAddr>) -> bool {
        let Some(peer) = peer else {
            return false;
        };
        // A replication group's security groups are its member clusters' (the group record
        // holds none): every member is created with the groups the request named.
        let groups = {
            let store = self.state.lock();
            let cluster_id = match store.replication_groups.get(&self.id) {
                Some(group) => group
                    .node_groups
                    .iter()
                    .flat_map(|shard| shard.members.iter())
                    .map(|member| member.cache_cluster_id.clone())
                    .next(),
                None => Some(self.id.clone()),
            };
            cluster_id
                .and_then(|id| store.cache_clusters.get(&id))
                .map(|cluster| cluster.security_group_ids.clone())
        };
        let Some(groups) = groups else {
            return false;
        };
        let store = self.firewall.lock();
        ::service_com_amazonaws_ec2_amazonec2::state::connection_firewall::store_admits_connection(&store, &groups, peer.ip(), self.port)
            == Some(false)
    }
}

impl ::emulator_server::NativeHandler for RedisProxy {
    fn serve(&self, conn: ::emulator_server::ListenerConn) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let container = Arc::clone(&self.container);
        let tls = self.tls.clone();
        let guard = self.guard.clone();
        Box::pin(async move {
            // A connection the cache's security groups do not admit is closed before anything
            // is read from it, as a dropped SYN never reaches the node on AWS.
            let peer = ::emulator_server::connection_peer();
            if guard.as_ref().is_some_and(|guard| guard.refuses(peer)) {
                tracing::warn!(container = %container.name(), ?peer, "the cache's security groups do not admit this connection");
                drop(conn);
                return;
            }
            if let Err(error) = proxy_connection(&container, tls, conn).await {
                tracing::debug!(container = %container.name(), %error, "a proxied cache connection ended with an error");
            }
        })
    }
}

/// One client connection: TLS when the cache has transit encryption, then the bytes passed
/// on to the container until either side closes.
async fn proxy_connection(
    container: &RedisContainer,
    tls: Option<::emulator_server::TlsUpgrader>,
    conn: ::emulator_server::ListenerConn,
) -> std::io::Result<()> {
    let mut conn = match tls {
        Some(upgrader) => upgrader.accept(conn, None).await?.0,
        None => conn,
    };
    let address = match tokio::time::timeout(SESSION_READY_WAIT, container.ready()).await {
        Ok(Ok(address)) => address,
        Ok(Err(reason)) => {
            let _ = conn.write_all(b"-ERR the cache's server is not running\r\n").await;
            return Err(std::io::Error::other(reason));
        }
        Err(_) => {
            let _ = conn.write_all(b"-LOADING the cache's server is starting\r\n").await;
            return Ok(());
        }
    };
    let mut upstream = tokio::net::TcpStream::connect(address).await?;
    tokio::io::copy_bidirectional(&mut conn, &mut upstream).await?;
    Ok(())
}

/// A container-backed cache's endpoint: the container and the emulator listener in front of
/// it. Dropping it closes the listener and removes the container.
pub struct RedisEndpoint {
    pub host: String,
    pub port: i32,
    container: Arc<RedisContainer>,
    _listener: ::emulator_server::ResourceListener,
}

impl std::fmt::Debug for RedisEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisEndpoint")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("container", &self.container)
            .finish_non_exhaustive()
    }
}

/// Start the container for the cache `id` and open the endpoint in front of it.
pub async fn open_endpoint<Eng: Send + Sync + 'static>(
    engine: &Eng,
    id: &str,
    engine_name: &str,
    engine_version: Option<&str>,
    password: Option<String>,
    transit_encryption: bool,
    port: i32,
    guard: Option<ConnectionGuard>,
) -> Result<RedisEndpoint, String> {
    let (image, server) =
        image_for(engine_name, engine_version).ok_or_else(|| format!("engine {engine_name} is not served by a container"))?;
    let listeners = (engine as &dyn std::any::Any)
        .downcast_ref::<::engine::InMemoryEngine>()
        .ok_or_else(|| "this emulator's engine has no listener surface".to_string())?
        .listeners();
    let tls = if transit_encryption {
        Some(
            listeners
                .tls_upgrader()
                .map_err(|error| format!("the cache endpoint's TLS could not be prepared: {error}"))?,
        )
    } else {
        None
    };
    let container = RedisContainer::start(ContainerSpec {
        name: container_name(id),
        image,
        server,
        password,
    });
    // The configured port when it is free (on another loopback address when 127.0.0.1
    // already serves it), so the endpoint reports the `Port` the cache was created with.
    let proxy = RedisProxy {
        container: Arc::clone(&container),
        tls,
        guard,
    };
    let listener = match u16::try_from(port) {
        Ok(preferred) => listeners.tcp_plaintext_preferring(proxy, preferred).await,
        Err(_) => listeners.tcp_plaintext(proxy).await,
    }
        .map_err(|error| format!("the cache endpoint could not be opened: {error}"))?;
    Ok(RedisEndpoint {
        host: listener.host(),
        port: i32::from(listener.port()),
        container,
        _listener: listener,
    })
}

/// What a create hands the background task that serves a new cache from a container.
#[derive(Clone)]
pub struct ServedCache {
    /// The replication group or cache cluster id.
    pub id: String,
    pub engine: String,
    pub engine_version: Option<String>,
    pub password: Option<String>,
    pub transit_encryption: bool,
    /// The cache's `Port` (6379 by default), which the endpoint listens on when it is free.
    pub port: i32,
}

impl std::fmt::Debug for ServedCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServedCache")
            .field("id", &self.id)
            .field("engine", &self.engine)
            .field("engine_version", &self.engine_version)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("transit_encryption", &self.transit_encryption)
            .field("port", &self.port)
            .finish()
    }
}

/// Serve `cache` from a container, in the background: open its endpoint, then attach it to
/// the cache's record ([`super::ComAmazonawsElasticacheAmazonelasticachev9State::attach_data_endpoint`]).
/// Nothing is awaited by the caller, which holds the store's guard. A failure is logged as
/// an error naming the cache, which then keeps the endpoints it was created with.
pub fn serve_in_background<Eng: Send + Sync + 'static>(
    engine: Arc<Eng>,
    state: ::engine::ServiceState<super::ComAmazonawsElasticacheAmazonelasticachev9State>,
    cache: ServedCache,
) {
    let guard = ::service_com_amazonaws_ec2_amazonec2::state::service_state(&*engine).map(|firewall| ConnectionGuard {
        state: state.clone(),
        id: cache.id.clone(),
        port: cache.port,
        firewall,
    });
    tokio::spawn(async move {
        match open_endpoint(
            &*engine,
            &cache.id,
            &cache.engine,
            cache.engine_version.as_deref(),
            cache.password.clone(),
            cache.transit_encryption,
            cache.port,
            guard,
        )
        .await
        {
            Ok(endpoint) => {
                tracing::info!(cache = %cache.id, host = %endpoint.host, port = endpoint.port, "the cache is served by a container");
                state.lock().attach_data_endpoint(&cache.id, endpoint);
            }
            Err(reason) => tracing::error!(cache = %cache.id, %reason, "the cache could not be served by a container"),
        }
    });
}

/// Whether a cache of `engine` is one a container serves when the backend is on.
#[must_use]
pub fn serves_engine(engine: &str) -> bool {
    image_for(engine, None).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_docker_turns_the_container_backend_on() {
        assert!(docker_backend_enabled(Some(" Docker ")));
        assert!(!docker_backend_enabled(Some("memory")));
        assert!(!docker_backend_enabled(None));
    }

    #[test]
    fn the_image_follows_the_engine_and_its_major_version() {
        assert_eq!(image_for("valkey", Some("8.0")), Some(("valkey/valkey:8".to_string(), "valkey-server")));
        assert_eq!(image_for("redis", Some("7.1")), Some(("redis:7".to_string(), "redis-server")));
        assert_eq!(image_for("Redis", None), Some(("redis:7".to_string(), "redis-server")));
        assert_eq!(image_for("memcached", Some("1.6")), None);
    }

    #[test]
    fn a_container_name_is_a_valid_docker_name() {
        assert_eq!(container_name("app-cache"), "aws-emulator-elasticache-app-cache");
    }

    #[test]
    fn pong_and_noauth_both_mean_the_server_runs() {
        assert!(is_ping_answer(b"+PONG\r\n"));
        assert!(is_ping_answer(b"-NOAUTH Authentication required.\r\n"));
        assert!(!is_ping_answer(b""));
    }

    #[test]
    fn the_auth_token_is_redacted_from_debug_output() {
        let spec = ContainerSpec {
            name: "n".to_string(),
            image: "valkey/valkey:8".to_string(),
            server: "valkey-server",
            password: Some("hunter2hunter2hunter2".to_string()),
        };
        assert!(!format!("{spec:?}").contains("hunter2"));
    }
}
