// SPDX-License-Identifier: CC0-1.0

//! A Kubernetes cluster running outside the emulator stands in for an EKS cluster's control
//! plane: once its service account key set is registered for the EKS cluster's issuer
//! (`PUT /_fork/eks/clusters/{name}/service-account-issuer-keys`), its tokens get AWS
//! credentials through IRSA (`sts:AssumeRoleWithWebIdentity`) and EKS Pod Identity
//! (`eks-auth:AssumeRoleForPodIdentity`), and a token it did not sign gets none.
//!
//! Every call goes through the router the binary serves, from `CreateCluster` to the token
//! exchange, so the EKS, IAM, STS and EKS Auth records involved are the ones a client sees.

use aws_emulator_fork_router::{build_router, InMemoryEngine};
use bytes::Bytes;
use emulator_server::EmulatorService;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REGION: &str = "us-east-1";
const CLUSTER: &str = "operations";
const NAMESPACE: &str = "kube-system";
const IRSA_ACCOUNT: &str = "external-dns";
const POD_IDENTITY_ACCOUNT: &str = "aws-load-balancer-controller";
const STS_AUDIENCE: &str = "sts.amazonaws.com";
const PODS_AUDIENCE: &str = "pods.eks.amazonaws.com";

/// Assembling the router needs a larger stack than a test thread has (see main.rs).
const ROUTER_STACK_SIZE: usize = 64 * 1024 * 1024;

type Router = emulator_server::WithExtraRoutes<smithy_server_runtime::MultiServiceRouter>;

fn on_router_thread(case: impl FnOnce(Router) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(ROUTER_STACK_SIZE)
        .spawn(move || {
            emulator_server::set_authentication(false);
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a tokio runtime");
            runtime.block_on(case(build_router(Arc::new(InMemoryEngine::new()))));
        })
        .expect("spawn the router thread")
        .join()
        .expect("the case passes");
}

/// The key a local cluster's API server signs service account tokens with.
struct ClusterKey {
    pair: EcdsaKeyPair,
}

impl ClusterKey {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).expect("a key");
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).expect("a key pair");
        Self { pair }
    }

    /// What `kubectl get --raw /openid/v1/jwks` answers.
    fn key_set(&self) -> Value {
        let point = self.pair.public_key().as_ref();
        json!({"keys": [{
            "use": "sig", "kty": "EC", "crv": "P-256", "alg": "ES256", "kid": "cluster-key",
            "x": base64url(&point[1..33]), "y": base64url(&point[33..65]),
        }]})
    }

    /// A projected service account token for `service_account` in [`NAMESPACE`].
    fn token(&self, issuer: &str, audience: &str, service_account: &str) -> String {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("after the epoch").as_secs();
        let header = json!({"alg": "ES256", "kid": "cluster-key"});
        let claims = json!({
            "iss": issuer, "aud": [audience], "iat": now, "nbf": now, "exp": now + 3600,
            "sub": format!("system:serviceaccount:{NAMESPACE}:{service_account}"),
            "kubernetes.io": {"namespace": NAMESPACE, "pod": {"name": format!("{service_account}-0"), "uid": "1"}, "serviceaccount": {"name": service_account, "uid": "2"}},
        });
        let signing_input = format!("{}.{}", base64url(header.to_string().as_bytes()), base64url(claims.to_string().as_bytes()));
        let signature = self.pair.sign(&SystemRandom::new(), signing_input.as_bytes()).expect("a signature");
        format!("{signing_input}.{}", base64url(signature.as_ref()))
    }
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut text = String::new();
    for chunk in bytes.chunks(3) {
        let bits = chunk.iter().fold(0u32, |acc, byte| (acc << 8) | u32::from(*byte)) << (8 * (3 - chunk.len()));
        for index in 0..=chunk.len() {
            text.push(char::from(ALPHABET[((bits >> (18 - 6 * index)) & 63) as usize]));
        }
    }
    text
}

fn form_encoded(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => char::from(byte).to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn authorization(service: &str) -> String {
    format!("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260101/{REGION}/{service}/aws4_request, SignedHeaders=host, Signature=0000")
}

/// A query-protocol call (EC2, IAM, STS); answers the status and the XML body.
async fn query(router: &Router, service: &str, host: &str, version: &str, action: &str, params: &[(&str, &str)]) -> (u16, String) {
    let mut body = format!("Action={action}&Version={version}");
    for (name, value) in params {
        body.push_str(&format!("&{name}={}", form_encoded(value)));
    }
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("https://{host}/"))
        .header("host", host)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("authorization", authorization(service))
        .body(Bytes::from(body))
        .expect("a well-formed request");
    let response = router.handle_request(request).await;
    (response.status().as_u16(), String::from_utf8_lossy(response.body()).to_string())
}

/// A REST JSON call (EKS, EKS Auth, the fork's registration); answers the status and body.
async fn rest(router: &Router, service: &str, host: &str, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let request = http::Request::builder()
        .method(method)
        .uri(format!("https://{host}{path}"))
        .header("host", host)
        .header("content-type", "application/json")
        .header("authorization", authorization(service))
        .body(Bytes::from(body.to_string()))
        .expect("a well-formed request");
    let response = router.handle_request(request).await;
    let status = response.status().as_u16();
    (status, serde_json::from_slice(response.body()).unwrap_or(Value::Null))
}

fn element(xml: &str, name: &str) -> String {
    let open = format!("<{name}>");
    let start = xml.find(&open).unwrap_or_else(|| panic!("no <{name}> in {xml}")) + open.len();
    let end = xml[start..].find('<').expect("a closing tag") + start;
    xml[start..end].to_string()
}

async fn ec2(router: &Router, action: &str, params: &[(&str, &str)]) -> String {
    let (status, body) = query(router, "ec2", &format!("ec2.{REGION}.amazonaws.com"), "2016-11-15", action, params).await;
    assert_eq!(status, 200, "{action}: {body}");
    body
}

async fn iam(router: &Router, action: &str, params: &[(&str, &str)]) -> String {
    let (status, body) = query(router, "iam", "iam.amazonaws.com", "2010-05-08", action, params).await;
    assert_eq!(status, 200, "{action}: {body}");
    body
}

async fn role(router: &Router, name: &str, trust: &Value) -> String {
    element(&iam(router, "CreateRole", &[("RoleName", name), ("AssumeRolePolicyDocument", &trust.to_string())]).await, "Arn")
}

/// An EKS cluster, an IAM OIDC provider for its issuer, the IRSA role and the pod identity
/// association a controller would be given, and the local cluster's key registered for it.
struct World {
    issuer: String,
    irsa_role: String,
    pod_role: String,
}

async fn world(router: &Router, key: &ClusterKey) -> World {
    let vpc = element(&ec2(router, "CreateVpc", &[("CidrBlock", "10.40.0.0/16")]).await, "vpcId");
    let mut subnets = Vec::new();
    for (cidr, zone) in [("10.40.1.0/24", "us-east-1a"), ("10.40.2.0/24", "us-east-1b")] {
        let created = ec2(router, "CreateSubnet", &[("VpcId", &vpc), ("CidrBlock", cidr), ("AvailabilityZone", zone)]).await;
        subnets.push(element(&created, "subnetId"));
    }
    let cluster_role = role(router, "cluster", &json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"Service": "eks.amazonaws.com"}, "Action": "sts:AssumeRole"}]})).await;
    let eks_host = format!("eks.{REGION}.amazonaws.com");
    let (status, created) = rest(router, "eks", &eks_host, "POST", "/clusters", &json!({"name": CLUSTER, "roleArn": cluster_role, "resourcesVpcConfig": {"subnetIds": subnets}})).await;
    assert_eq!(status, 200, "CreateCluster: {created}");
    let mut issuer = created["cluster"]["identity"]["oidc"]["issuer"].as_str().unwrap_or_default().to_string();
    for _ in 0..50 {
        let (_, described) = rest(router, "eks", &eks_host, "GET", &format!("/clusters/{CLUSTER}"), &Value::Null).await;
        issuer = described["cluster"]["identity"]["oidc"]["issuer"].as_str().unwrap_or_default().to_string();
        if described["cluster"]["status"] == json!("ACTIVE") && !issuer.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(issuer.starts_with("https://oidc.eks."), "the cluster reports issuer {issuer:?}");

    let (status, registered) = rest(router, "eks", &eks_host, "PUT", &format!("/_fork/eks/clusters/{CLUSTER}/service-account-issuer-keys"), &key.key_set()).await;
    assert_eq!((status, registered["issuer"].as_str()), (200, Some(issuer.as_str())), "{registered}");

    iam(router, "CreateOpenIDConnectProvider", &[("Url", &issuer), ("ClientIDList.member.1", STS_AUDIENCE), ("ThumbprintList.member.1", "9e99a48a9960b14926bb7f3b02e22da2b0ab7280")]).await;
    let provider = issuer.trim_start_matches("https://").to_string();
    let account = engine::emulator_account_id();
    let irsa_role = role(router, "external-dns", &json!({"Version": "2012-10-17", "Statement": [{
        "Effect": "Allow",
        "Principal": {"Federated": format!("arn:aws:iam::{account}:oidc-provider/{provider}")},
        "Action": "sts:AssumeRoleWithWebIdentity",
        "Condition": {"StringEquals": {format!("{provider}:sub"): format!("system:serviceaccount:{NAMESPACE}:{IRSA_ACCOUNT}"), format!("{provider}:aud"): STS_AUDIENCE}},
    }]})).await;
    let pod_role = role(router, "load-balancer-controller", &json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"Service": PODS_AUDIENCE}, "Action": ["sts:AssumeRole", "sts:TagSession"]}]})).await;
    let (status, associated) = rest(router, "eks", &eks_host, "POST", &format!("/clusters/{CLUSTER}/pod-identity-associations"), &json!({"namespace": NAMESPACE, "serviceAccount": POD_IDENTITY_ACCOUNT, "roleArn": pod_role})).await;
    assert_eq!(status, 200, "CreatePodIdentityAssociation: {associated}");
    World { issuer, irsa_role, pod_role }
}

async fn assume_with_web_identity(router: &Router, role_arn: &str, token: &str) -> (u16, String) {
    query(router, "sts", &format!("sts.{REGION}.amazonaws.com"), "2011-06-15", "AssumeRoleWithWebIdentity", &[("RoleArn", role_arn), ("RoleSessionName", "irsa"), ("WebIdentityToken", token)]).await
}

async fn assume_for_pod_identity(router: &Router, token: &str) -> (u16, Value) {
    rest(router, "eks-auth", &format!("eks-auth.{REGION}.api.aws"), "POST", &format!("/clusters/{CLUSTER}/assume-role-for-pod-identity"), &json!({"token": token})).await
}

#[test]
fn an_irsa_token_the_local_cluster_signed_assumes_the_role() {
    on_router_thread(|router| {
        Box::pin(async move {
            let key = ClusterKey::new();
            let world = world(&router, &key).await;

            let (status, body) = assume_with_web_identity(&router, &world.irsa_role, &key.token(&world.issuer, STS_AUDIENCE, IRSA_ACCOUNT)).await;

            assert_eq!(status, 200, "{body}");
            assert!(element(&body, "Arn").contains(":assumed-role/external-dns/irsa"), "{body}");
        })
    });
}

#[test]
fn an_irsa_token_signed_by_another_key_is_refused() {
    on_router_thread(|router| {
        Box::pin(async move {
            let key = ClusterKey::new();
            let world = world(&router, &key).await;

            let forged = ClusterKey::new().token(&world.issuer, STS_AUDIENCE, IRSA_ACCOUNT);
            let (status, body) = assume_with_web_identity(&router, &world.irsa_role, &forged).await;

            assert_eq!(status, 400, "{body}");
            assert!(body.contains("InvalidIdentityToken"), "{body}");
        })
    });
}

#[test]
fn a_pod_identity_token_the_local_cluster_signed_gets_the_associated_role() {
    on_router_thread(|router| {
        Box::pin(async move {
            let key = ClusterKey::new();
            let world = world(&router, &key).await;

            let (status, body) = assume_for_pod_identity(&router, &key.token(&world.issuer, PODS_AUDIENCE, POD_IDENTITY_ACCOUNT)).await;

            assert_eq!(status, 200, "{body}");
            assert_eq!(body["assumedRoleUser"]["arn"], json!(world.pod_role), "{body}");
            assert_eq!(body["subject"], json!({"namespace": NAMESPACE, "serviceAccount": POD_IDENTITY_ACCOUNT}), "{body}");
            assert!(body["credentials"]["accessKeyId"].as_str().is_some_and(|id| !id.is_empty()), "{body}");
        })
    });
}

#[test]
fn a_pod_identity_token_signed_by_another_key_is_refused() {
    on_router_thread(|router| {
        Box::pin(async move {
            let key = ClusterKey::new();
            let world = world(&router, &key).await;

            let forged = ClusterKey::new().token(&world.issuer, PODS_AUDIENCE, POD_IDENTITY_ACCOUNT);
            let (status, body) = assume_for_pod_identity(&router, &forged).await;

            assert_eq!(status, 400, "{body}");
        })
    });
}

#[test]
fn a_pod_identity_token_for_a_service_account_with_no_association_is_denied() {
    on_router_thread(|router| {
        Box::pin(async move {
            let key = ClusterKey::new();
            let world = world(&router, &key).await;

            let (status, body) = assume_for_pod_identity(&router, &key.token(&world.issuer, PODS_AUDIENCE, "unassociated")).await;

            assert_eq!(status, 403, "{body}");
        })
    });
}

#[test]
fn a_key_set_for_a_cluster_the_emulator_does_not_hold_is_not_registered() {
    on_router_thread(|router| {
        Box::pin(async move {
            let key = ClusterKey::new();

            let (status, body) = rest(&router, "eks", &format!("eks.{REGION}.amazonaws.com"), "PUT", "/_fork/eks/clusters/absent/service-account-issuer-keys", &key.key_set()).await;

            assert_eq!(status, 404, "{body}");
        })
    });
}
