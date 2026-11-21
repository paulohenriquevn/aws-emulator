// SPDX-License-Identifier: CC0-1.0

//! With `EMULATOR_EKS_CONTROL_PLANE=external`, an EKS cluster's control plane is provisioned
//! outside the emulator (a kind cluster, for instance), as Amazon EKS provisions a real one:
//! `CreateCluster` leaves the cluster `CREATING`, the provisioner reads the issuer it must
//! sign under and the certificate authority it must sign with from
//! `GET /_fork/eks/clusters/{name}/control-plane`, and its
//! `PUT /_fork/eks/clusters/{name}/control-plane` with the API server's endpoint is what
//! makes the cluster `ACTIVE`. From then on `DescribeCluster` reports that endpoint beside
//! the certificate the API server chains to, so a kubeconfig built from them (Terraform's
//! `kubernetes` and `helm` providers, `aws eks update-kubeconfig`) reaches the real API
//! server and trusts it.
//!
//! The mode is read once per process, so this file holds the one test binary that runs
//! with it on; every test here sets it before the router is built.

use aws_emulator_fork_router::{build_router, InMemoryEngine};
use bytes::Bytes;
use emulator_server::EmulatorService;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const REGION: &str = "us-east-1";
const CLUSTER: &str = "core";
const ENDPOINT: &str = "https://172.29.10.10:6443";

/// Assembling the router needs a larger stack than a test thread has (see main.rs).
const ROUTER_STACK_SIZE: usize = 64 * 1024 * 1024;

type Router = emulator_server::WithExtraRoutes<smithy_server_runtime::MultiServiceRouter>;

fn on_router_thread(case: impl FnOnce(Router) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(ROUTER_STACK_SIZE)
        .spawn(move || {
            // Every test of this binary sets the same value, so a concurrent read never
            // observes another one.
            std::env::set_var("EMULATOR_EKS_CONTROL_PLANE", "external");
            emulator_server::set_authentication(false);
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a tokio runtime");
            runtime.block_on(case(build_router(Arc::new(InMemoryEngine::new()))));
        })
        .expect("spawn the router thread")
        .join()
        .expect("the case passes");
}

fn authorization(service: &str) -> String {
    format!("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260101/{REGION}/{service}/aws4_request, SignedHeaders=host, Signature=0000")
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

async fn query(router: &Router, service: &str, host: &str, version: &str, action: &str, params: &[(&str, &str)]) -> String {
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
    let text = String::from_utf8_lossy(response.body()).to_string();
    assert_eq!(response.status().as_u16(), 200, "{action}: {text}");
    text
}

async fn rest(router: &Router, method: &str, path: &str, body: &Value) -> (u16, Value) {
    let host = format!("eks.{REGION}.amazonaws.com");
    let request = http::Request::builder()
        .method(method)
        .uri(format!("https://{host}{path}"))
        .header("host", &host)
        .header("content-type", "application/json")
        .header("authorization", authorization("eks"))
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

/// A VPC with two subnets, the cluster role, and `CreateCluster`.
async fn create_cluster(router: &Router) {
    let ec2 = |action: &'static str, params: Vec<(&'static str, String)>| async move {
        let params: Vec<(&str, &str)> = params.iter().map(|(name, value)| (*name, value.as_str())).collect();
        query(router, "ec2", &format!("ec2.{REGION}.amazonaws.com"), "2016-11-15", action, &params).await
    };
    let vpc = element(&ec2("CreateVpc", vec![("CidrBlock", "10.41.0.0/16".into())]).await, "vpcId");
    let mut subnets = Vec::new();
    for (cidr, zone) in [("10.41.1.0/24", "us-east-1a"), ("10.41.2.0/24", "us-east-1b")] {
        let created = ec2("CreateSubnet", vec![("VpcId", vpc.clone()), ("CidrBlock", cidr.into()), ("AvailabilityZone", zone.into())]).await;
        subnets.push(element(&created, "subnetId"));
    }
    let trust = json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Principal": {"Service": "eks.amazonaws.com"}, "Action": "sts:AssumeRole"}]});
    let role = element(
        &query(router, "iam", "iam.amazonaws.com", "2010-05-08", "CreateRole", &[("RoleName", "core-cluster"), ("AssumeRolePolicyDocument", &trust.to_string())]).await,
        "Arn",
    );
    let (status, created) = rest(router, "POST", "/clusters", &json!({"name": CLUSTER, "roleArn": role, "resourcesVpcConfig": {"subnetIds": subnets}})).await;
    assert_eq!(status, 200, "CreateCluster: {created}");
}

async fn described(router: &Router) -> Value {
    let (status, body) = rest(router, "GET", &format!("/clusters/{CLUSTER}"), &Value::Null).await;
    assert_eq!(status, 200, "DescribeCluster: {body}");
    body["cluster"].clone()
}

/// Longer than the emulator's own activation delay, so a cluster the clock would activate
/// has been activated by the time the case looks.
async fn past_the_activation_delay() {
    tokio::time::sleep(Duration::from_millis(1500)).await;
}

#[test]
fn an_external_control_plane_cluster_stays_creating_until_it_is_registered() {
    on_router_thread(|router| {
        Box::pin(async move {
            create_cluster(&router).await;
            past_the_activation_delay().await;

            let cluster = described(&router).await;

            assert_eq!(cluster["status"], json!("CREATING"), "{cluster}");
            assert_eq!(cluster["endpoint"], Value::Null, "{cluster}");
        })
    });
}

#[test]
fn the_provisioner_reads_the_issuer_of_a_creating_cluster() {
    on_router_thread(|router| {
        Box::pin(async move {
            create_cluster(&router).await;

            let (status, plane) = rest(&router, "GET", &format!("/_fork/eks/clusters/{CLUSTER}/control-plane"), &Value::Null).await;

            assert_eq!(status, 200, "{plane}");
            assert_eq!(plane["status"], json!("CREATING"), "{plane}");
            assert!(plane["issuer"].as_str().unwrap_or_default().starts_with("https://oidc.eks.us-east-1.amazonaws.com/id/"), "{plane}");
            assert!(plane["version"].as_str().is_some_and(|version| !version.is_empty()), "{plane}");
            assert!(plane["certificateAuthority"]["signingKey"].as_str().unwrap_or_default().contains("PRIVATE KEY"), "the CA's signing key is handed to the provisioner");
        })
    });
}

#[test]
fn registering_the_control_plane_activates_the_cluster_with_its_endpoint_and_certificate() {
    on_router_thread(|router| {
        Box::pin(async move {
            create_cluster(&router).await;

            let (_, plane) = rest(&router, "GET", &format!("/_fork/eks/clusters/{CLUSTER}/control-plane"), &Value::Null).await;

            let (status, registered) = rest(&router, "PUT", &format!("/_fork/eks/clusters/{CLUSTER}/control-plane"), &json!({"endpoint": ENDPOINT})).await;

            assert_eq!(status, 200, "{registered}");
            let cluster = described(&router).await;
            assert_eq!(cluster["status"], json!("ACTIVE"), "{cluster}");
            assert_eq!(cluster["endpoint"], json!(ENDPOINT), "{cluster}");
            assert_eq!(cluster["certificateAuthority"]["data"], plane["certificateAuthority"]["data"], "the CA the API server signs with is the one the cluster reports");
            assert!(cluster["identity"]["oidc"]["issuer"].is_string(), "{cluster}");
        })
    });
}

#[test]
fn a_registration_whose_endpoint_is_not_https_is_refused_and_the_cluster_stays_creating() {
    on_router_thread(|router| {
        Box::pin(async move {
            create_cluster(&router).await;

            let (status, refused) = rest(
                &router,
                "PUT",
                &format!("/_fork/eks/clusters/{CLUSTER}/control-plane"),
                &json!({"endpoint": "http://172.29.10.10:6443"}),
            )
            .await;

            assert_eq!(status, 400, "{refused}");
            assert_eq!(described(&router).await["status"], json!("CREATING"));
        })
    });
}

#[test]
fn a_registration_for_a_cluster_that_does_not_exist_is_not_found() {
    on_router_thread(|router| {
        Box::pin(async move {
            let (status, refused) = rest(
                &router,
                "PUT",
                "/_fork/eks/clusters/nowhere/control-plane",
                &json!({"endpoint": ENDPOINT}),
            )
            .await;

            assert_eq!(status, 404, "{refused}");
        })
    });
}
