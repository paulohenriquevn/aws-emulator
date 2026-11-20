// SPDX-License-Identifier: CC0-1.0

//! fork: a Valkey replication group served by a real Valkey server in a container
//! (`EMULATOR_ELASTICACHE_BACKEND=docker`, `src/state/redis_container.rs`).
//!
//! Each case creates a group through the ElastiCache API, connects to the primary endpoint
//! it publishes and speaks RESP to it. Needs Docker, so every case is `#[ignore]`d and CI
//! runs them on their own with `cargo test --test all redis_container -- --ignored`. The
//! cases run one at a time, and never beside the rest of the suite, because the backend is
//! chosen by an environment variable of this process.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use service_com_amazonaws_elasticache_amazonelasticachev9::state::redis_container::BACKEND_ENV;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::harness::{params, Emulator};

static SERIAL: Mutex<()> = Mutex::new(());

fn container_backend() -> std::sync::MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // A process-wide write: the cases hold SERIAL and run apart from the rest of the suite.
    std::env::set_var(BACKEND_ENV, "docker");
    guard
}

const TOKEN: &str = "a-token-of-sixteen-or-more-chars";

/// Create a Valkey group and answer the host and port its primary endpoint reports once the
/// group is `available` with the container-backed endpoint attached (an address that is not
/// an `amazonaws.com` name). A group still `creating` refuses `DeleteReplicationGroup`.
async fn served_group(emulator: &Emulator, id: &str, extra: &[(&str, &str)]) -> (String, u16) {
    let mut pairs = params(&[
        ("ReplicationGroupId", id),
        ("ReplicationGroupDescription", "a container-backed group"),
        ("Engine", "valkey"),
        ("EngineVersion", "8.0"),
        ("CacheNodeType", "cache.t4g.micro"),
    ]);
    pairs.extend(params(extra));
    let created = emulator.call("CreateReplicationGroup", &pairs).await;
    assert_eq!(created.status, 200, "CreateReplicationGroup: {}", created.body);
    for _ in 0..100 {
        let described = emulator
            .call("DescribeReplicationGroups", &params(&[("ReplicationGroupId", id)]))
            .await;
        if let (Some(address), Some(port), Some(status)) = (
            described.text("PrimaryEndpoint/Address"),
            described.text("PrimaryEndpoint/Port"),
            described.text("ReplicationGroup/Status"),
        ) {
            if !address.ends_with("amazonaws.com") && status == "available" {
                return (address.to_string(), port.parse().expect("a port number"));
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the group never became available with the container-backed listener as its primary endpoint");
}

/// One RESP command and the first reply line, retried while the server is still starting
/// (the image pull on a first run takes longer than one connection waits).
async fn command_over<S, F>(connect: F, command: &[&str]) -> String
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = S>>>,
{
    for _ in 0..60 {
        let mut stream = connect().await;
        let reply = exchange(&mut stream, &[command]).await;
        if !reply[0].starts_with("-LOADING") && !reply[0].is_empty() {
            return reply[0].clone();
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    panic!("the server never left LOADING");
}

/// Send `commands` on one connection and answer the first line of each reply.
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, commands: &[&[&str]]) -> Vec<String> {
    let mut replies = Vec::new();
    for command in commands {
        let mut frame = format!("*{}\r\n", command.len());
        for part in *command {
            frame.push_str(&format!("${}\r\n{part}\r\n", part.len()));
        }
        stream.write_all(frame.as_bytes()).await.expect("a written command");
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte).await {
                Ok(1) => {}
                _ => break,
            }
            line.push(byte[0]);
            if line.ends_with(b"\r\n") {
                break;
            }
        }
        let mut text = String::from_utf8_lossy(&line).trim_end().to_string();
        // A bulk string's header is followed by its value.
        if let Some(length) = text.strip_prefix('$').and_then(|n| n.parse::<usize>().ok()) {
            let mut value = vec![0u8; length + 2];
            stream.read_exact(&mut value).await.expect("a bulk value");
            text = String::from_utf8_lossy(&value[..length]).to_string();
        }
        replies.push(text);
    }
    replies
}

type TlsStream = tokio_rustls::client::TlsStream<TcpStream>;

/// A TLS connection to the endpoint of a group with transit encryption (`AuthToken` needs it,
/// as on AWS).
async fn tls_connection(host: &str, port: u16) -> TlsStream {
    let tcp = TcpStream::connect((host, port)).await.expect("a connection");
    tls_connector()
        .connect(rustls::pki_types::ServerName::try_from("localhost").expect("a name"), tcp)
        .await
        .expect("a TLS handshake with the endpoint")
}

fn over_tls(host: String, port: u16) -> impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = TlsStream>>> {
    move || {
        let host = host.clone();
        Box::pin(async move { tls_connection(&host, port).await })
    }
}

/// The settings of a group with an `AuthToken`, which AWS accepts only with transit encryption.
const WITH_AUTH: [(&str, &str); 2] = [("AuthToken", TOKEN), ("TransitEncryptionEnabled", "true")];

fn plain(host: String, port: u16) -> impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = TcpStream>>> {
    move || {
        let host = host.clone();
        Box::pin(async move { TcpStream::connect((host.as_str(), port)).await.expect("a connection to the endpoint") })
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with --ignored"]
async fn a_command_without_auth_is_refused_with_noauth() {
    let _backend = container_backend();
    let emulator = Emulator::new();
    let (host, port) = served_group(&emulator, "noauth-group", &WITH_AUTH).await;

    let reply = command_over(over_tls(host, port), &["GET", "k"]).await;

    assert!(reply.starts_with("-NOAUTH"), "{reply:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with --ignored"]
async fn a_value_set_through_the_primary_endpoint_is_read_back() {
    let _backend = container_backend();
    let emulator = Emulator::new();
    let (host, port) = served_group(&emulator, "set-get-group", &WITH_AUTH).await;
    command_over(over_tls(host.clone(), port), &["AUTH", TOKEN]).await;
    let mut stream = tls_connection(&host, port).await;

    let replies = exchange(&mut stream, &[&["AUTH", TOKEN][..], &["SET", "k", "v"][..], &["GET", "k"][..]]).await;

    assert_eq!(replies, vec!["+OK".to_string(), "+OK".to_string(), "v".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with --ignored"]
async fn a_group_with_transit_encryption_is_reached_over_tls() {
    let _backend = container_backend();
    let emulator = Emulator::new();
    let (host, port) = served_group(&emulator, "tls-group", &WITH_AUTH).await;

    let reply = command_over(over_tls(host, port), &["PING"]).await;

    assert!(reply.starts_with("-NOAUTH"), "a server answered over TLS: {reply:?}");
}

/// The Terraform AWS provider reads a group's `port` back from its primary endpoint, so an
/// endpoint on any other port than the configured one is drift on every plan.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with --ignored"]
async fn the_primary_endpoint_reports_the_configured_port() {
    let _backend = container_backend();
    let emulator = Emulator::new();

    let (host, port) = served_group(&emulator, "port-group", &[("Port", "6390")]).await;

    assert_eq!(port, 6390, "the endpoint {host}:{port} is not on the group's Port");
    command_over(plain(host, port), &["PING"]).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with --ignored"]
async fn a_deleted_group_leaves_no_container_behind() {
    let _backend = container_backend();
    let emulator = Emulator::new();
    let (host, port) = served_group(&emulator, "deleted-group", &[]).await;
    command_over(plain(host, port), &["PING"]).await;

    let deleted = emulator
        .call("DeleteReplicationGroup", &params(&[("ReplicationGroupId", "deleted-group")]))
        .await;
    assert_eq!(deleted.status, 200, "{}", deleted.body);

    let name = "aws-emulator-elasticache-deleted-group";
    for _ in 0..60 {
        let listed = std::process::Command::new("docker")
            .args(["ps", "-a", "--filter", &format!("name=^{name}$"), "--format", "{{.Names}}"])
            .output()
            .expect("docker ps");
        if String::from_utf8_lossy(&listed.stdout).trim().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("container {name} is still there after DeleteReplicationGroup");
}

/// The cache's security group, with one inbound rule: TCP `rule_port` from the client's group,
/// which a network interface of the account at 127.0.0.1 (the address the test connects
/// from) carries. Answers the cache's group.
async fn guard_cache(emulator: &Emulator, rule_port: i32) -> String {
    use service_com_amazonaws_ec2_amazonec2::ops::types as ec2_types;
    let cache_group = emulator.security_group_named("cache").await;
    let client_group = emulator.security_group_named("client").await;
    emulator
        .ec2(crate::harness::REGION)
        .authorize_security_group_ingress()
        .group_id(&cache_group)
        .ip_permissions(
            ::aws_sdk_ec2::types::IpPermission::builder()
                .ip_protocol("tcp")
                .from_port(rule_port)
                .to_port(rule_port)
                .user_id_group_pairs(::aws_sdk_ec2::types::UserIdGroupPair::builder().group_id(&client_group).build())
                .build(),
        )
        .send()
        .await
        .expect("EC2 must add the inbound rule");
    let state = service_com_amazonaws_ec2_amazonec2::state::service_state(&**emulator.engine()).expect("the EC2 store");
    state.lock().network_interfaces.insert(
        "eni-0e1a5c0e1a5c00001".to_string(),
        ec2_types::NetworkInterface {
            network_interface_id: Some("eni-0e1a5c0e1a5c00001".to_string()),
            private_ip_address: Some("127.0.0.1".to_string()),
            groups: Some(vec![ec2_types::GroupIdentifier {
                group_id: Some(client_group),
                group_name: Some("client".to_string()),
            }]),
            ..ec2_types::NetworkInterface::default()
        },
    );
    cache_group
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with --ignored"]
async fn a_connection_the_security_groups_do_not_admit_is_closed_unanswered() {
    let _backend = container_backend();
    let emulator = Emulator::new();
    // The rule names another port than the cache's, so nothing admits the client.
    let cache_group = guard_cache(&emulator, 6394).await;
    let (host, port) = served_group(
        &emulator,
        "refused-group",
        &[("Port", "6393"), ("SecurityGroupIds.SecurityGroupId.1", &cache_group)],
    )
    .await;

    let mut stream = TcpStream::connect((host.as_str(), port)).await.expect("a connection to the endpoint");
    let reply = tokio::time::timeout(Duration::from_secs(10), exchange(&mut stream, &[&["PING"][..]]))
        .await
        .expect("the endpoint neither answered nor closed the connection");

    assert_eq!(reply, vec![String::new()], "the endpoint answered a client its security groups do not admit");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with --ignored"]
async fn a_connection_the_security_groups_admit_reaches_the_server() {
    let _backend = container_backend();
    let emulator = Emulator::new();
    let cache_group = guard_cache(&emulator, 6395).await;
    let (host, port) = served_group(
        &emulator,
        "admitted-group",
        &[("Port", "6395"), ("SecurityGroupIds.SecurityGroupId.1", &cache_group)],
    )
    .await;

    let reply = command_over(plain(host, port), &["PING"]).await;

    assert_eq!(reply, "+PONG");
}

/// A container dropped after a newer one took its name (a group deleted and one of the same
/// id created, as `terraform apply` after a `destroy` does) must remove its own container
/// only: the newer one keeps serving.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with --ignored"]
async fn a_dropped_container_leaves_a_newer_one_of_its_name_running() {
    use service_com_amazonaws_elasticache_amazonelasticachev9::state::redis_container::{
        container_name, image_for, ContainerSpec, RedisContainer,
    };
    let _backend = container_backend();
    let (image, server) = image_for("valkey", Some("8.0")).expect("a Valkey image");
    let spec = ContainerSpec { name: container_name("shared-name"), image, server, password: None };
    let older = RedisContainer::start(spec.clone());
    older.ready().await.expect("the older container is ready");
    let newer = RedisContainer::start(spec.clone());
    let address = newer.ready().await.expect("the newer container is ready");

    drop(older);
    tokio::time::sleep(Duration::from_secs(3)).await;

    let listed = std::process::Command::new("docker")
        .args(["ps", "-q", "--filter", &format!("name=^{}$", spec.name)])
        .output()
        .expect("docker ps");
    assert!(
        !String::from_utf8_lossy(&listed.stdout).trim().is_empty(),
        "dropping the older container removed the newer {}",
        spec.name
    );
    TcpStream::connect(address).await.expect("the newer container still accepts connections");
    drop(newer);
}

/// A TLS client that accepts the emulator's certificate whatever it names: what is checked
/// here is that the endpoint speaks TLS, not the certificate chain.
fn tls_connector() -> tokio_rustls::TlsConnector {
    #[derive(Debug)]
    struct AnyCertificate(Arc<rustls::crypto::CryptoProvider>);
    impl rustls::client::danger::ServerCertVerifier for AnyCertificate {
        fn verify_server_cert(
            &self,
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
        }
        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .expect("the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}
