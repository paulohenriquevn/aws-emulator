// SPDX-License-Identifier: CC0-1.0

//! fork: a DB instance whose engine is PostgreSQL, served by a real PostgreSQL server.
//!
//! With [`BACKEND_ENV`] set to `docker`, `CreateDBInstance` for engine `postgres` starts a
//! `postgres:<major version>` container instead of the in-process SQLite engine. The master
//! user, its password and `DBName` are the ones the request gave (or the password RDS
//! generated for a managed secret). The container listens on a loopback port only; what the
//! instance publishes as its `Endpoint` is still a listener of this emulator, and every
//! connection it accepts is passed on byte for byte to the container
//! ([`PostgresContainerProxy`]). So a client speaks to PostgreSQL itself (its SQL, its
//! extensions, SCRAM authentication and TLS), while the published port, the instance status
//! and the security groups stay decisions of this emulator.
//!
//! What the proxy does before passing a connection on, with the rules the in-process engine
//! applies ([`super::wire_session::precheck`]): it answers `SSLRequest` and runs TLS itself
//! with the emulator's certificate (so `sslmode=verify-full` works against the emulator's CA
//! bundle), refuses a session the instance is not `available` for, and refuses a plaintext
//! session when `rds.force_ssl` is on, with PostgreSQL's own error for each. The startup
//! message then goes to the container, and from there on the bytes flow unchanged. A reboot
//! or a stop severs the connections already open. The container itself runs without TLS: it
//! is reached on loopback only.
//!
//! The container is removed when the instance's endpoint is dropped, which is what
//! `DeleteDBInstance` does. Containers carry the label [`CONTAINER_LABEL`], so ones left
//! behind by an emulator that was killed can be found and removed.

use std::net::SocketAddr;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::watch;


/// The environment variable that chooses the backend of PostgreSQL DB instances.
pub const BACKEND_ENV: &str = "EMULATOR_RDS_BACKEND";

/// The environment variable that names the image repository the containers run, for a
/// mirror or a private registry. The tag is always the instance's major version.
pub const IMAGE_ENV: &str = "EMULATOR_RDS_POSTGRES_IMAGE";

/// The image repository when [`IMAGE_ENV`] is unset.
pub const DEFAULT_IMAGE_REPOSITORY: &str = "postgres";

/// The major version a container runs when the request names none: the RDS default for
/// PostgreSQL.
pub const DEFAULT_MAJOR_VERSION: &str = "17";

/// The label every container this module starts carries.
pub const CONTAINER_LABEL: &str = "aws-emulator.service=rds";

/// How long a container may take to accept connections, image pull included.
const READY_TIMEOUT: Duration = Duration::from_secs(300);

/// How often readiness is probed while the container starts.
const READY_POLL: Duration = Duration::from_millis(500);

/// The port PostgreSQL listens on inside the container.
const CONTAINER_PORT: &str = "5432/tcp";

/// Whether `value` of [`BACKEND_ENV`] turns the container backend on: `docker`, in any
/// case, surrounding whitespace ignored. Anything else keeps the in-process engine.
#[must_use]
pub fn docker_backend_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.trim().eq_ignore_ascii_case("docker"))
}

/// Whether the process is configured for the container backend.
#[must_use]
pub fn docker_backend_configured() -> bool {
    docker_backend_enabled(std::env::var(BACKEND_ENV).ok().as_deref())
}

/// The image a DB instance of `engine_version` runs: `<repository>:<major version>`, where
/// the major version is the part before the first dot (`16.4` runs `16`).
#[must_use]
pub fn image_for(repository: &str, engine_version: Option<&str>) -> String {
    let major = engine_version
        .map(str::trim)
        .and_then(|version| version.split('.').next())
        .filter(|major| !major.is_empty())
        .unwrap_or(DEFAULT_MAJOR_VERSION);
    format!("{repository}:{major}")
}

/// The container name for the DB instance with `resource_id` (`db-...`): Docker names allow
/// `[a-zA-Z0-9_.-]`, which a resource id already is.
#[must_use]
pub fn container_name(resource_id: &str) -> String {
    let safe: String = resource_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    format!("aws-emulator-rds-{safe}")
}

/// What the container is started with.
#[derive(Clone)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub master_username: String,
    pub master_password: String,
    /// `DBName`, or `None` for an instance created without one, which has only the
    /// `postgres` database.
    pub database: Option<String>,
}

impl std::fmt::Debug for ContainerSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerSpec")
            .field("name", &self.name)
            .field("image", &self.image)
            .field("master_username", &self.master_username)
            .field("master_password", &"<redacted>")
            .field("database", &self.database)
            .finish()
    }
}

/// Where a container has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerState {
    Starting,
    Ready(SocketAddr),
    Failed(String),
}

/// A PostgreSQL container started for one DB instance. Dropping it removes the container.
pub struct PostgresContainer {
    name: String,
    state: watch::Receiver<ContainerState>,
}

impl std::fmt::Debug for PostgresContainer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresContainer")
            .field("name", &self.name)
            .field("state", &*self.state.borrow())
            .finish()
    }
}

impl PostgresContainer {
    /// Start the container `spec` describes, in the background: the call returns at once,
    /// and [`Self::ready`] answers when the server accepts connections.
    #[must_use]
    pub fn start(spec: ContainerSpec) -> Arc<Self> {
        let (sender, receiver) = watch::channel(ContainerState::Starting);
        let name = spec.name.clone();
        tokio::spawn(async move {
            let outcome = run_container(&spec).await;
            if let Err(reason) = &outcome {
                tracing::error!(container = %spec.name, image = %spec.image, %reason, "the PostgreSQL container did not start");
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
        let waited = tokio::time::timeout(READY_TIMEOUT, async {
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
        })
        .await;
        waited.unwrap_or_else(|_| Err(format!("container {} was not ready within {READY_TIMEOUT:?}", self.name)))
    }

    /// Change the master user's password in the server, as `ModifyDBInstance` with
    /// `MasterUserPassword` does, without awaiting anything (the caller holds the store's
    /// guard in scope). A server that already runs is changed before this returns, so the
    /// next session logs in with the new password; one still starting is changed by a task
    /// once it is ready. A failure is logged as an error naming the container.
    pub fn change_master_password(self: &Arc<Self>, username: String, password: String) {
        let ready = matches!(&*self.state.borrow(), ContainerState::Ready(_));
        if ready {
            log_password_outcome(&self.name, run_password_change(&self.name, &username, &password));
            return;
        }
        let container = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = match container.ready().await {
                Ok(_) => {
                    let name = container.name.clone();
                    tokio::task::spawn_blocking(move || run_password_change(&name, &username, &password))
                        .await
                        .unwrap_or_else(|error| Err(format!("the password change task failed: {error}")))
                }
                Err(reason) => Err(reason),
            };
            log_password_outcome(&container.name, outcome);
        });
    }
}

fn log_password_outcome(name: &str, outcome: Result<(), String>) {
    if let Err(reason) = outcome {
        tracing::error!(
            container = %name,
            %reason,
            "the master password of the PostgreSQL container was not changed, so the old \
             password still opens a session on it"
        );
    }
}

/// `ALTER USER` in the container `name`, through `psql`. The password travels on the
/// standard input of `psql` as a variable, never on a command line.
fn run_password_change(name: &str, username: &str, password: &str) -> Result<(), String> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("docker")
        .args(["exec", "-i", name, "psql", "-v", "ON_ERROR_STOP=1", "-q", "-U", username, "-d", "postgres"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("docker exec could not be started: {error}"))?;
    let script = format!(
        "\\set user {}\n\\set password {}\nALTER USER :\"user\" PASSWORD :'password';\n",
        psql_literal(username),
        psql_literal(password)
    );
    let mut stdin = child.stdin.take().ok_or("docker exec has no standard input")?;
    stdin
        .write_all(script.as_bytes())
        .map_err(|error| format!("writing to psql: {error}"))?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .map_err(|error| format!("waiting for psql: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "psql in {name} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

impl Drop for PostgresContainer {
    fn drop(&mut self) {
        let name = self.name.clone();
        // Removal must not block the request that dropped the endpoint, so it runs on a
        // thread of its own, which also reaps the docker process.
        std::thread::spawn(move || {
            match std::process::Command::new("docker")
                .args(["rm", "-f", "-v", &name])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
            {
                Ok(output) if output.status.success() => {
                    tracing::debug!(container = %name, "removed the PostgreSQL container");
                }
                Ok(output) => tracing::warn!(
                    container = %name,
                    stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                    "docker rm of the PostgreSQL container failed"
                ),
                Err(error) => tracing::warn!(container = %name, %error, "docker rm could not be started"),
            }
        });
    }
}

/// A psql meta-command argument: single-quoted, where psql reads `''` as a quote and `\\`
/// as a backslash.
fn psql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}

/// `docker run` the container, then wait until the server accepts TCP connections inside
/// it. Answers the loopback address Docker published.
async fn run_container(spec: &ContainerSpec) -> Result<SocketAddr, String> {
    // A container of the same name is one an earlier emulator left behind.
    let _ = Command::new("docker")
        .args(["rm", "-f", "-v", &spec.name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    let database = spec.database.clone().unwrap_or_else(|| "postgres".to_string());
    let output = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            &spec.name,
            "--label",
            CONTAINER_LABEL,
            // The values come from this process's environment, so the password is on no
            // command line.
            "-e",
            "POSTGRES_USER",
            "-e",
            "POSTGRES_PASSWORD",
            "-e",
            "POSTGRES_DB",
            "-p",
            "127.0.0.1::5432",
            &spec.image,
        ])
        .env("POSTGRES_USER", &spec.master_username)
        .env("POSTGRES_PASSWORD", &spec.master_password)
        .env("POSTGRES_DB", &database)
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
    // The image's entrypoint first runs a server on the Unix socket only, to initialize the
    // database, then restarts it on TCP: a TCP check inside the container passes only for
    // the server clients reach.
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        let probe = Command::new("docker")
            .args(["exec", &spec.name, "pg_isready", "-q", "-h", "127.0.0.1", "-p", "5432", "-U", &spec.master_username])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|error| format!("docker exec pg_isready could not be started: {error}"))?;
        if probe.success() {
            tracing::info!(container = %spec.name, image = %spec.image, %address, "the PostgreSQL container accepts connections");
            return Ok(address);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("{} did not accept connections within {READY_TIMEOUT:?}", spec.name));
        }
        tokio::time::sleep(READY_POLL).await;
    }
}

/// The loopback address Docker published the container's PostgreSQL port on.
async fn published_address(name: &str) -> Result<SocketAddr, String> {
    let output = Command::new("docker")
        .args(["port", name, CONTAINER_PORT])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| format!("docker port could not be started: {error}"))?;
    let listed = String::from_utf8_lossy(&output.stdout);
    parse_published_address(&listed).ok_or_else(|| {
        format!(
            "docker port {name} {CONTAINER_PORT} named no IPv4 address: {:?} {}",
            listed.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })
}

/// The first IPv4 `address:port` line of `docker port` output.
#[must_use]
pub fn parse_published_address(listed: &str) -> Option<SocketAddr> {
    listed
        .lines()
        .filter_map(|line| line.trim().parse::<SocketAddr>().ok())
        .find(SocketAddr::is_ipv4)
}

/// How long a session waits for a container that is still starting before it is refused
/// as a server that is starting up.
const SESSION_READY_WAIT: Duration = Duration::from_secs(30);

/// The handler of a container-backed DB instance's endpoint.
pub struct PostgresContainerProxy {
    pub(super) container: Arc<PostgresContainer>,
    /// What the in-process engine admits a session against: the store, the instance, the
    /// core's TLS server and the run a reboot or a stop bumps.
    pub(super) context: super::wire_session::SessionContext,
}

impl ::emulator_server::NativeHandler for PostgresContainerProxy {
    fn serve(&self, conn: ::emulator_server::ListenerConn) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let container = Arc::clone(&self.container);
        let context = self.context.clone();
        Box::pin(async move {
            let name = container.name().to_string();
            if let Err(error) = proxy_session(context, container, conn).await {
                tracing::debug!(container = %name, %error, "a proxied PostgreSQL session ended with an error");
            }
        })
    }
}

/// One client connection: the requests before the startup message, the admission checks,
/// then the bytes passed on to the container until either side closes.
async fn proxy_session(
    context: super::wire_session::SessionContext,
    container: Arc<PostgresContainer>,
    conn: ::emulator_server::ListenerConn,
) -> std::io::Result<()> {
    use super::pg_wire::{fatal, read_startup, CANCEL_REQUEST_CODE, GSSENC_REQUEST_CODE, PROTOCOL_VERSION_3, SSL_REQUEST_CODE};
    let mut conn = conn;
    let mut encrypted = false;
    let startup = loop {
        let body = read_startup(&mut conn).await?;
        let code = i32::from_be_bytes([
            *body.first().unwrap_or(&0),
            *body.get(1).unwrap_or(&0),
            *body.get(2).unwrap_or(&0),
            *body.get(3).unwrap_or(&0),
        ]);
        match code {
            SSL_REQUEST_CODE if body.len() == 4 && !encrypted => {
                conn.write_all(b"S").await?;
                conn.flush().await?;
                let (upgraded, _alpn) = context.upgrader.accept(conn, None).await?;
                conn = upgraded;
                encrypted = true;
                continue;
            }
            GSSENC_REQUEST_CODE if body.len() == 4 => {
                conn.write_all(b"N").await?;
                conn.flush().await?;
                continue;
            }
            // A cancel request names a backend of the container's own server, so it goes
            // to that server as it came.
            CANCEL_REQUEST_CODE => {
                let address = container.ready().await.map_err(std::io::Error::other)?;
                let mut upstream = tokio::net::TcpStream::connect(address).await?;
                upstream.write_all(&framed(&body)).await?;
                return upstream.shutdown().await;
            }
            PROTOCOL_VERSION_3 => break body,
            _ => {
                return fatal(&mut conn, "0A000", &format!("unsupported startup request {code}")).await;
            }
        }
    };
    let Some(facts) = context.endpoint_facts() else {
        return fatal(&mut conn, "57P03", "the database system is starting up").await;
    };
    let parameters = super::pg_wire::startup_parameters(startup.get(4..).unwrap_or_default());
    let user = parameters
        .iter()
        .find(|(key, _)| key == "user")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    let database = parameters
        .iter()
        .find(|(key, _)| key == "database")
        .map(|(_, value)| value.clone());
    if let Err(refusal) = super::wire_session::precheck(&facts, encrypted) {
        let (code, message) = super::pg_wire::refusal_sentence(&refusal, &facts, &user, database.as_deref());
        return fatal(&mut conn, code, &message).await;
    }
    let address = match tokio::time::timeout(SESSION_READY_WAIT, container.ready()).await {
        Ok(Ok(address)) => address,
        Ok(Err(reason)) => {
            tracing::warn!(container = %container.name(), %reason, "the PostgreSQL container is not running");
            return fatal(&mut conn, "57P01", "the database system is not running").await;
        }
        Err(_) => return fatal(&mut conn, "57P03", "the database system is starting up").await,
    };
    let mut upstream = tokio::net::TcpStream::connect(address).await?;
    upstream.write_all(&framed(&startup)).await?;
    let mut generation = context.generation.clone();
    generation.borrow_and_update();
    tokio::select! {
        copied = tokio::io::copy_bidirectional(&mut conn, &mut upstream) => {
            copied?;
        }
        _ = generation.changed() => {
            tracing::info!(container = %container.name(), "the DB instance rebooted or stopped, so an open session was severed");
        }
    }
    Ok(())
}

/// A startup-phase packet as it travels: its length (itself included), then its body.
fn framed(body: &[u8]) -> Vec<u8> {
    let length = i32::try_from(body.len() + 4).unwrap_or(i32::MAX);
    let mut packet = Vec::with_capacity(body.len() + 4);
    packet.extend_from_slice(&length.to_be_bytes());
    packet.extend_from_slice(body);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_docker_turns_the_container_backend_on() {
        assert!(docker_backend_enabled(Some("docker")));
        assert!(docker_backend_enabled(Some(" Docker ")));
        assert!(!docker_backend_enabled(None));
        assert!(!docker_backend_enabled(Some("sqlite")));
        assert!(!docker_backend_enabled(Some("")));
    }

    #[test]
    fn the_image_tag_is_the_major_version() {
        assert_eq!(image_for("postgres", Some("16.4")), "postgres:16");
        assert_eq!(image_for("postgres", Some("17")), "postgres:17");
        assert_eq!(image_for("mirror/postgres", None), "mirror/postgres:17");
        assert_eq!(image_for("postgres", Some("")), "postgres:17");
    }

    #[test]
    fn a_container_name_is_a_valid_docker_name() {
        assert_eq!(container_name("db-ABC123"), "aws-emulator-rds-db-abc123");
        assert_eq!(container_name("db-a/b"), "aws-emulator-rds-db-a-b");
    }

    #[test]
    fn the_published_ipv4_address_is_read_from_docker_port() {
        assert_eq!(
            parse_published_address("127.0.0.1:49153\n[::1]:49153\n"),
            Some("127.0.0.1:49153".parse().unwrap())
        );
        assert_eq!(parse_published_address(""), None);
    }

    #[test]
    fn a_startup_packet_is_framed_with_its_length() {
        assert_eq!(framed(&[1, 2]), vec![0, 0, 0, 6, 1, 2]);
    }

    #[test]
    fn a_psql_literal_escapes_quotes_and_backslashes() {
        assert_eq!(psql_literal(r"a'b\c"), r"'a''b\\c'");
    }

    #[test]
    fn the_password_is_redacted_from_debug_output() {
        let spec = ContainerSpec {
            name: "n".to_string(),
            image: "postgres:17".to_string(),
            master_username: "app".to_string(),
            master_password: "hunter2".to_string(),
            database: None,
        };
        assert!(!format!("{spec:?}").contains("hunter2"));
    }
}
