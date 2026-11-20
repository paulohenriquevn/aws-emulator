// SPDX-License-Identifier: CC0-1.0

//! fork: a PostgreSQL DB instance served by a real PostgreSQL server in a container
//! (`EMULATOR_RDS_BACKEND=docker`, `src/state/data_plane/postgres_container.rs`).
//!
//! Each case creates an instance through the RDS API, connects to the endpoint it publishes
//! the way libpq does (TLS by `SSLRequest`, then SCRAM-SHA-256), and checks what only a real
//! server answers: the major version, an extension the in-process engine does not have.
//! Needs Docker, so every case is `#[ignore]`d and CI runs them on their own with
//! `cargo test --test all postgres_container -- --ignored`. The cases run one at a time, and
//! never beside the rest of the suite, because the backend is chosen by an environment
//! variable of this process.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use hmac::{Hmac, Mac};
use service_com_amazonaws_rds_amazonrdsv19::state::data_plane::postgres_container::{self, BACKEND_ENV};
use service_com_amazonaws_rds_amazonrdsv19::EmulatedService;
use sha2::{Digest, Sha256};
use smithy_server_runtime::MultiServiceRouter;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

const REGION: &str = "us-east-1";
const MASTER_USER: &str = "app";
const MASTER_PASSWORD: &str = "first-Secret-42";
const DATABASE: &str = "app";

/// The cases share this process's environment, so they run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn container_backend() -> std::sync::MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var(BACKEND_ENV, "docker");
    guard
}

fn emulator() -> MultiServiceRouter {
    emulator_on(Arc::new(engine::InMemoryEngine::new()))
}

fn emulator_on(engine: Arc<engine::InMemoryEngine>) -> MultiServiceRouter {
    let mut router = MultiServiceRouter::new();
    router.set_request_authentication(false);
    router.register_service(EmulatedService::new(engine));
    router
}

/// One awsQuery call, answering the status and the XML body.
async fn call(router: &MultiServiceRouter, action: &str, params: &[(&str, &str)]) -> (u16, String) {
    let mut body = format!("Action={action}&Version=2014-10-31");
    for (name, value) in params {
        body.push('&');
        body.push_str(name);
        body.push('=');
        body.push_str(&form_encode(value));
    }
    let request = http::Request::builder()
        .method("POST")
        .uri("https://rds.amazonaws.com/")
        .header("content-type", "application/x-www-form-urlencoded")
        .header(
            "authorization",
            format!(
                "AWS4-HMAC-SHA256 Credential=AKIDEMULATOR/20260101/{REGION}/rds/aws4_request, \
                 SignedHeaders=host, Signature=unverified"
            ),
        )
        .body(bytes::Bytes::from(body))
        .expect("a well-formed request");
    let response = router.dispatch(request).await;
    (
        response.status().as_u16(),
        String::from_utf8_lossy(response.body()).into_owned(),
    )
}

fn form_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// The text of the first `<tag>` in `xml`.
fn text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(xml[start..end].to_string())
}

/// Create an instance of `engine_version` and wait until it is `available`. Answers the
/// endpoint's port and the instance's resource id.
async fn available_instance(router: &MultiServiceRouter, identifier: &str, engine_version: &str) -> (u16, String) {
    available_instance_with(router, identifier, engine_version, &[]).await
}

async fn available_instance_with(
    router: &MultiServiceRouter,
    identifier: &str,
    engine_version: &str,
    extra: &[(&str, &str)],
) -> (u16, String) {
    let mut params = vec![
        ("DBInstanceIdentifier", identifier),
        ("Engine", "postgres"),
        ("EngineVersion", engine_version),
        ("DBInstanceClass", "db.t4g.micro"),
        ("AllocatedStorage", "20"),
        ("MasterUsername", MASTER_USER),
        ("MasterUserPassword", MASTER_PASSWORD),
        ("DBName", DATABASE),
    ];
    params.extend_from_slice(extra);
    let (status, body) = call(router, "CreateDBInstance", &params).await;
    assert_eq!(status, 200, "CreateDBInstance: {body}");
    let resource_id = text(&body, "DbiResourceId").expect("a DbiResourceId");
    for _ in 0..600 {
        let (_, described) = call(router, "DescribeDBInstances", &[("DBInstanceIdentifier", identifier)]).await;
        if text(&described, "DBInstanceStatus").as_deref() == Some("available") {
            let endpoint = text(&described, "Endpoint").expect("an Endpoint");
            let port = text(&endpoint, "Port").expect("an endpoint port").parse().expect("a numeric port");
            return (port, resource_id);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{identifier} did not become available");
}

trait Connection: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Connection for T {}
type Client = Box<dyn Connection>;

/// What the server answered a login with, when it refused it.
#[derive(Debug)]
struct Refusal {
    code: String,
    message: String,
}

/// [`login_once`], again while the server answers that it is starting up: the first case to
/// run pulls the image, which can take longer than the endpoint waits for the container.
async fn login(port: u16, tls: bool, user: &str, password: &str) -> Result<Client, Refusal> {
    for _ in 0..60 {
        match login_once(port, tls, user, password).await {
            Err(refusal) if refusal.code == "57P03" => tokio::time::sleep(Duration::from_secs(5)).await,
            outcome => return outcome,
        }
    }
    login_once(port, tls, user, password).await
}

/// Log in on `127.0.0.1:port`, over TLS when `tls`, and answer the open session.
async fn login_once(port: u16, tls: bool, user: &str, password: &str) -> Result<Client, Refusal> {
    let mut conn: Client = Box::new(TcpStream::connect(("127.0.0.1", port)).await.expect("the endpoint accepts a connection"));
    if tls {
        let mut request = Vec::new();
        request.extend_from_slice(&8i32.to_be_bytes());
        request.extend_from_slice(&80_877_103i32.to_be_bytes());
        conn.write_all(&request).await.expect("SSLRequest sent");
        let mut answer = [0u8; 1];
        conn.read_exact(&mut answer).await.expect("an answer to SSLRequest");
        assert_eq!(answer[0], b'S', "the endpoint must offer TLS");
        conn = upgrade_to_tls(conn).await;
    }
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    for (key, value) in [("user", user), ("database", DATABASE)] {
        put_cstring(&mut body, key);
        put_cstring(&mut body, value);
    }
    body.push(0);
    let mut startup = (i32::try_from(body.len() + 4).unwrap()).to_be_bytes().to_vec();
    startup.extend_from_slice(&body);
    conn.write_all(&startup).await.expect("startup sent");

    let mut scram: Option<Scram> = None;
    loop {
        let (kind, body) = read_message(&mut conn).await;
        match kind {
            b'R' => {
                let code = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                match code {
                    0 => {}
                    10 => {
                        let mechanisms = String::from_utf8_lossy(&body[4..]).to_string();
                        assert!(mechanisms.contains("SCRAM-SHA-256"), "the server must offer SCRAM-SHA-256, got {mechanisms:?}");
                        let state = Scram::new();
                        let first = state.client_first();
                        let mut message = Vec::new();
                        put_cstring(&mut message, "SCRAM-SHA-256");
                        message.extend_from_slice(&i32::try_from(first.len()).unwrap().to_be_bytes());
                        message.extend_from_slice(first.as_bytes());
                        send(&mut conn, b'p', &message).await;
                        scram = Some(state);
                    }
                    11 => {
                        let server_first = String::from_utf8_lossy(&body[4..]).to_string();
                        let state = scram.as_ref().expect("SASLContinue follows SASL");
                        let final_message = state.client_final(password, &server_first);
                        send(&mut conn, b'p', final_message.as_bytes()).await;
                    }
                    12 => {}
                    other => panic!("unexpected authentication request {other}"),
                }
            }
            b'E' => return Err(refusal(&body)),
            b'Z' => return Ok(conn),
            _ => {}
        }
    }
}

/// Run one simple query and answer the first column of every row.
async fn query(conn: &mut Client, sql: &str) -> Result<Vec<String>, Refusal> {
    let mut message = Vec::new();
    put_cstring(&mut message, sql);
    send(conn, b'Q', &message).await;
    let mut rows = Vec::new();
    let mut failed = None;
    loop {
        let (kind, body) = read_message(conn).await;
        match kind {
            b'D' => {
                let length = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                rows.push(if length < 0 {
                    String::new()
                } else {
                    String::from_utf8_lossy(&body[6..6 + length as usize]).to_string()
                });
            }
            b'E' => failed = Some(refusal(&body)),
            b'Z' => return failed.map_or(Ok(rows), Err),
            _ => {}
        }
    }
}

async fn send(conn: &mut Client, kind: u8, body: &[u8]) {
    let mut message = vec![kind];
    message.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
    message.extend_from_slice(body);
    conn.write_all(&message).await.expect("message sent");
}

async fn read_message(conn: &mut Client) -> (u8, Vec<u8>) {
    let mut head = [0u8; 5];
    conn.read_exact(&mut head).await.expect("a message from the server");
    let length = i32::from_be_bytes([head[1], head[2], head[3], head[4]]);
    let mut body = vec![0u8; usize::try_from(length - 4).expect("a valid length")];
    conn.read_exact(&mut body).await.expect("the message body");
    (head[0], body)
}

fn put_cstring(body: &mut Vec<u8>, value: &str) {
    body.extend_from_slice(value.as_bytes());
    body.push(0);
}

fn refusal(body: &[u8]) -> Refusal {
    let mut code = String::new();
    let mut message = String::new();
    for field in body.split(|byte| *byte == 0) {
        match field.first() {
            Some(b'C') => code = String::from_utf8_lossy(&field[1..]).to_string(),
            Some(b'M') => message = String::from_utf8_lossy(&field[1..]).to_string(),
            _ => {}
        }
    }
    Refusal { code, message }
}

/// The client half of SCRAM-SHA-256 (RFC 5802, RFC 7677), as PostgreSQL runs it.
struct Scram {
    nonce: String,
}

impl Scram {
    fn new() -> Self {
        let nonce: [u8; 18] = std::array::from_fn(|index| (index as u8).wrapping_mul(37).wrapping_add(11));
        Self { nonce: base64::engine::general_purpose::STANDARD.encode(nonce) }
    }

    fn client_first_bare(&self) -> String {
        format!("n=,r={}", self.nonce)
    }

    fn client_first(&self) -> String {
        format!("n,,{}", self.client_first_bare())
    }

    fn client_final(&self, password: &str, server_first: &str) -> String {
        let field = |name: &str| {
            server_first
                .split(',')
                .find_map(|part| part.strip_prefix(&format!("{name}=")))
                .unwrap_or_else(|| panic!("server-first-message lacks {name}: {server_first}"))
                .to_string()
        };
        let nonce = field("r");
        assert!(nonce.starts_with(&self.nonce), "the server nonce must extend the client's");
        let salt = base64::engine::general_purpose::STANDARD.decode(field("s")).expect("a base64 salt");
        let iterations: u32 = field("i").parse().expect("an iteration count");
        let salted = pbkdf2_sha256(password.as_bytes(), &salt, iterations);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = Sha256::digest(client_key);
        let without_proof = format!("c=biws,r={nonce}");
        let auth_message = format!("{},{server_first},{without_proof}", self.client_first_bare());
        let signature = hmac_sha256(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(signature.iter()).map(|(a, b)| a ^ b).collect();
        format!("{without_proof},p={}", base64::engine::general_purpose::STANDARD.encode(proof))
    }
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut block = salt.to_vec();
    block.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password, &block);
    let mut result = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for (out, byte) in result.iter_mut().zip(u.iter()) {
            *out ^= byte;
        }
    }
    result
}

/// The client side of TLS, accepting the certificate the emulator's own core generated.
async fn upgrade_to_tls(conn: Client) -> Client {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .expect("the default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptTheEmulatorsCertificate(provider)))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let server_name = rustls::pki_types::ServerName::try_from("localhost").expect("a server name");
    Box::new(connector.connect(server_name, conn).await.expect("the TLS handshake completes"))
}

#[derive(Debug)]
struct AcceptTheEmulatorsCertificate(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptTheEmulatorsCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
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

/// Whether Docker still holds a container named `name`.
fn container_exists(name: &str) -> bool {
    let output = std::process::Command::new("docker")
        .args(["ps", "-a", "-q", "--filter", &format!("name=^{name}$")])
        .output()
        .expect("docker ps runs");
    !String::from_utf8_lossy(&output.stdout).trim().is_empty()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with --ignored"]
#[allow(clippy::await_holding_lock)]
async fn the_endpoint_is_a_real_postgres_server_of_the_requested_major_version() {
    let _serial = container_backend();
    let router = emulator();
    let (port, _) = available_instance(&router, "pg-version", "16").await;

    let mut session = login(port, true, MASTER_USER, MASTER_PASSWORD).await.expect("the master user logs in over TLS");
    let version = query(&mut session, "SELECT version()").await.expect("version() answers");
    let extension = query(&mut session, "CREATE EXTENSION IF NOT EXISTS pgcrypto").await;
    let digest = query(&mut session, "SELECT encode(digest('abc', 'sha256'), 'hex')").await;

    assert!(version[0].starts_with("PostgreSQL 16"), "got {version:?}");
    assert!(extension.is_ok(), "pgcrypto must install: {extension:?}");
    assert_eq!(
        digest.expect("digest() answers"),
        vec!["ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with --ignored"]
#[allow(clippy::await_holding_lock)]
async fn a_plaintext_session_is_refused_while_rds_force_ssl_is_on() {
    let _serial = container_backend();
    let router = emulator();
    // PostgreSQL 15 and later default `rds.force_ssl` to 1.
    let (port, _) = available_instance(&router, "pg-force-ssl", "16").await;

    let refused = login(port, false, MASTER_USER, MASTER_PASSWORD).await.err().expect("a plaintext login is refused");

    assert_eq!(refused.code, "28000", "got {refused:?}");
    assert!(refused.message.contains("no encryption"), "got {refused:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with --ignored"]
#[allow(clippy::await_holding_lock)]
async fn a_modified_master_password_is_the_one_that_logs_in() {
    let _serial = container_backend();
    let router = emulator();
    let (port, _) = available_instance(&router, "pg-password", "16").await;
    login(port, true, MASTER_USER, MASTER_PASSWORD).await.expect("the first password logs in");

    let (status, body) = call(
        &router,
        "ModifyDBInstance",
        &[("DBInstanceIdentifier", "pg-password"), ("MasterUserPassword", "second-Secret-43"), ("ApplyImmediately", "true")],
    )
    .await;
    assert_eq!(status, 200, "ModifyDBInstance: {body}");
    for _ in 0..600 {
        let (_, described) = call(&router, "DescribeDBInstances", &[("DBInstanceIdentifier", "pg-password")]).await;
        if text(&described, "DBInstanceStatus").as_deref() == Some("available") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let old = login(port, true, MASTER_USER, MASTER_PASSWORD).await;
    let new = login(port, true, MASTER_USER, "second-Secret-43").await;
    assert_eq!(old.err().map(|refusal| refusal.code), Some("28P01".to_string()));
    assert!(new.is_ok(), "the new password must log in: {:?}", new.err());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with --ignored"]
#[allow(clippy::await_holding_lock)]
async fn a_deleted_instance_leaves_no_container_behind() {
    let _serial = container_backend();
    let router = emulator();
    let (port, resource_id) = available_instance(&router, "pg-delete", "16").await;
    let name = postgres_container::container_name(&resource_id);
    // The instance reports `available` on its own clock, while the first case to run may
    // still be pulling the image; a login waits for the server.
    login(port, true, MASTER_USER, MASTER_PASSWORD).await.expect("the master user logs in over TLS");
    assert!(container_exists(&name), "{name} runs while the instance exists");

    let (status, body) = call(
        &router,
        "DeleteDBInstance",
        &[("DBInstanceIdentifier", "pg-delete"), ("SkipFinalSnapshot", "true")],
    )
    .await;
    assert_eq!(status, 200, "DeleteDBInstance: {body}");
    let mut gone = false;
    for _ in 0..300 {
        // A describe makes the clock's due transitions, the removal among them.
        let _ = call(&router, "DescribeDBInstances", &[("DBInstanceIdentifier", "pg-delete")]).await;
        if !container_exists(&name) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(gone, "{name} must be removed once the instance is deleted");
}

/// The security group of the DB instance, and the one of the client's network interface.
const DATABASE_GROUP: &str = "sg-0d0b0d0b0d0b00001";
const CLIENT_GROUP: &str = "sg-0d0b0d0b0d0b00002";

/// EC2 holds the client's network interface at 127.0.0.1 (the address the test connects
/// from) with `CLIENT_GROUP`, and `DATABASE_GROUP` with one inbound rule: TCP `port` from
/// `CLIENT_GROUP`.
fn guard_database(engine: &engine::InMemoryEngine, port: i32) {
    use service_com_amazonaws_ec2_amazonec2::ops::types as ec2_types;
    let state = service_com_amazonaws_ec2_amazonec2::state::service_state(engine).expect("the EC2 store");
    let mut store = state.lock();
    store.security_groups.insert(
        DATABASE_GROUP.to_string(),
        ec2_types::SecurityGroup {
            group_id: Some(DATABASE_GROUP.to_string()),
            group_name: Some("database".to_string()),
            ip_permissions: Some(vec![ec2_types::IpPermission {
                ip_protocol: Some("tcp".to_string()),
                from_port: Some(port),
                to_port: Some(port),
                user_id_group_pairs: Some(vec![ec2_types::UserIdGroupPair {
                    group_id: Some(CLIENT_GROUP.to_string()),
                    ..ec2_types::UserIdGroupPair::default()
                }]),
                ..ec2_types::IpPermission::default()
            }]),
            ..ec2_types::SecurityGroup::default()
        },
    );
    store.network_interfaces.insert(
        "eni-0d0b0d0b0d0b00001".to_string(),
        ec2_types::NetworkInterface {
            network_interface_id: Some("eni-0d0b0d0b0d0b00001".to_string()),
            private_ip_address: Some("127.0.0.1".to_string()),
            groups: Some(vec![ec2_types::GroupIdentifier {
                group_id: Some(CLIENT_GROUP.to_string()),
                group_name: Some("client".to_string()),
            }]),
            ..ec2_types::NetworkInterface::default()
        },
    );
}

/// What the endpoint answers an `SSLRequest` with: `Some(b'S')` from a server, `None` when
/// the connection is closed without an answer.
async fn ssl_answer(port: u16) -> Option<u8> {
    let mut conn = TcpStream::connect(("127.0.0.1", port)).await.expect("a TCP connection to the endpoint");
    conn.write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f]).await.ok()?;
    let mut answer = [0u8; 1];
    match tokio::time::timeout(Duration::from_secs(10), conn.read(&mut answer)).await {
        Ok(Ok(1)) => Some(answer[0]),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with --ignored"]
#[allow(clippy::await_holding_lock)]
async fn a_connection_the_security_groups_do_not_admit_is_closed_before_the_handshake() {
    let _serial = container_backend();
    let engine = Arc::new(engine::InMemoryEngine::new());
    let router = emulator_on(Arc::clone(&engine));
    // The rules are read for the instance's `Port`, which the listener prefers but may not
    // get; the rule here names another port, so nothing admits the client.
    let (port, _) = available_instance_with(
        &router,
        "pg-guarded",
        "16",
        &[("Port", "5441"), ("VpcSecurityGroupIds.VpcSecurityGroupId.1", DATABASE_GROUP)],
    )
    .await;
    guard_database(&engine, 5442);

    let answer = ssl_answer(port).await;

    assert_eq!(answer, None, "the endpoint answered a client its security groups do not admit");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker; run with --ignored"]
#[allow(clippy::await_holding_lock)]
async fn a_connection_the_security_groups_admit_reaches_the_server() {
    let _serial = container_backend();
    let engine = Arc::new(engine::InMemoryEngine::new());
    let router = emulator_on(Arc::clone(&engine));
    // The rule names the instance's `Port`, whatever port the listener got.
    let (port, _) = available_instance_with(
        &router,
        "pg-admitted",
        "16",
        &[("Port", "5443"), ("VpcSecurityGroupIds.VpcSecurityGroupId.1", DATABASE_GROUP)],
    )
    .await;
    guard_database(&engine, 5443);

    let answer = ssl_answer(port).await;

    assert_eq!(answer, Some(b'S'));
}
