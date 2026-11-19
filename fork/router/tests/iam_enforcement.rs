// SPDX-License-Identifier: CC0-1.0

//! With `EMULATOR_IAM_ENFORCE=on`, a call signed by an assumed role passes only when the
//! role's identity-based policies allow its action (`fork/iam-gate`), while the emulator's own
//! credential passes without any policy.
//!
//! The router runs with signature checks off, so a request is signed with the session's
//! access key id alone.

use aws_emulator_fork_router::{build_services, InMemoryEngine};
use bytes::Bytes;
use serde_json::json;
use smithy_server_runtime::MultiServiceRouter;
use std::sync::Arc;

const REGION: &str = "us-east-1";
const ADMINISTRATOR_KEY: &str = "AKIDEXAMPLE";

/// Assembling the router needs a larger stack than a test thread has (see main.rs).
const ROUTER_STACK_SIZE: usize = 64 * 1024 * 1024;

fn on_enforcing_router(case: impl FnOnce(MultiServiceRouter) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(ROUTER_STACK_SIZE)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(ROUTER_STACK_SIZE)
                .enable_all()
                .build()
                .expect("a tokio runtime");
            // Every case of this binary turns the gate on, so the process-wide write is the
            // same value whichever case runs first.
            std::env::set_var(aws_emulator_fork_iam_gate::ENFORCE_ENV, "on");
            let mut router = build_services(Arc::new(InMemoryEngine::new()));
            router.set_request_authentication(false);
            runtime.block_on(case(router));
        })
        .expect("spawn the router thread")
        .join()
        .expect("the case passes");
}

fn authorization(access_key_id: &str, service: &str) -> String {
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key_id}/20260101/{REGION}/{service}/aws4_request, \
         SignedHeaders=host, Signature=0000"
    )
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// One form-encoded query call; answers the status and the body.
async fn query(
    router: &MultiServiceRouter,
    access_key_id: &str,
    service: &str,
    host: &str,
    action: &str,
    version: &str,
    params: &[(&str, &str)],
) -> (u16, String) {
    let mut body = format!("Action={action}&Version={version}");
    for (key, value) in params {
        body.push_str(&format!("&{key}={}", urlencode(value)));
    }
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("https://{host}/"))
        .header("host", host)
        .header("authorization", authorization(access_key_id, service))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Bytes::from(body))
        .expect("a well-formed request");
    let response = router.dispatch(request).await;
    (response.status().as_u16(), String::from_utf8_lossy(response.body()).into_owned())
}

async fn iam(router: &MultiServiceRouter, action: &str, params: &[(&str, &str)]) -> String {
    let (status, body) = query(router, ADMINISTRATOR_KEY, "iam", "iam.amazonaws.com", action, "2010-05-08", params).await;
    assert_eq!(status, 200, "{action}: {body}");
    body
}

fn element(xml: &str, name: &str) -> String {
    xml.split(&format!("<{name}>"))
        .nth(1)
        .and_then(|rest| rest.split(&format!("</{name}>")).next())
        .unwrap_or_else(|| panic!("no <{name}> in {xml}"))
        .to_string()
}

async fn describe_db_instances(router: &MultiServiceRouter, access_key_id: &str) -> (u16, String) {
    query(router, access_key_id, "rds", &format!("rds.{REGION}.amazonaws.com"), "DescribeDBInstances", "2014-10-31", &[]).await
}

async fn describe_repositories(router: &MultiServiceRouter, access_key_id: &str) -> (u16, String) {
    let request = http::Request::builder()
        .method("POST")
        .uri("/")
        .header("host", format!("api.ecr.{REGION}.amazonaws.com"))
        .header("authorization", authorization(access_key_id, "ecr"))
        .header("content-type", "application/x-amz-json-1.1")
        .header("x-amz-target", "AmazonEC2ContainerRegistry_V20150921.DescribeRepositories")
        .body(Bytes::from("{}"))
        .expect("a well-formed request");
    let response = router.dispatch(request).await;
    (response.status().as_u16(), String::from_utf8_lossy(response.body()).into_owned())
}

/// A role `name` with the inline policy `policy`, and the access key id of a session of it.
async fn session_of(router: &MultiServiceRouter, name: &str, policy: serde_json::Value) -> String {
    let trust = json!({
        "Version": "2012-10-17",
        "Statement": [{"Effect": "Allow", "Principal": {"AWS": "*"}, "Action": "sts:AssumeRole"}]
    });
    let created = iam(router, "CreateRole", &[("RoleName", name), ("AssumeRolePolicyDocument", &trust.to_string())]).await;
    iam(router, "PutRolePolicy", &[("RoleName", name), ("PolicyName", "inline"), ("PolicyDocument", &policy.to_string())]).await;
    let role_arn = element(&created, "Arn");
    let (status, body) = query(
        router,
        ADMINISTRATOR_KEY,
        "sts",
        &format!("sts.{REGION}.amazonaws.com"),
        "AssumeRole",
        "2011-06-15",
        &[("RoleArn", &role_arn), ("RoleSessionName", "gate")],
    )
    .await;
    assert_eq!(status, 200, "AssumeRole: {body}");
    element(&body, "AccessKeyId")
}

fn allow(action: &str) -> serde_json::Value {
    json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow", "Action": action, "Resource": "*"}]})
}

#[test]
fn a_role_without_the_action_is_refused() {
    on_enforcing_router(|router| {
        Box::pin(async move {
            let key = session_of(&router, "no-rds", allow("s3:ListAllMyBuckets")).await;

            let (status, body) = describe_db_instances(&router, &key).await;

            assert_eq!(status, 403, "{body}");
            assert!(body.contains("AccessDenied"), "{body}");
        })
    });
}

#[test]
fn a_role_allowed_the_action_passes() {
    on_enforcing_router(|router| {
        Box::pin(async move {
            let key = session_of(&router, "rds-reader", allow("rds:DescribeDBInstances")).await;

            let (status, body) = describe_db_instances(&router, &key).await;

            assert_eq!(status, 200, "{body}");
        })
    });
}

#[test]
fn an_explicit_deny_wins_over_an_allow_of_everything() {
    on_enforcing_router(|router| {
        Box::pin(async move {
            let policy = json!({
                "Version": "2012-10-17",
                "Statement": [
                    {"Effect": "Allow", "Action": "*", "Resource": "*"},
                    {"Effect": "Deny", "Action": "ecr:*", "Resource": "*"}
                ]
            });
            let key = session_of(&router, "no-ecr", policy).await;

            let (status, body) = describe_repositories(&router, &key).await;

            assert_eq!(status, 403, "{body}");
            assert!(body.contains("AccessDenied"), "{body}");
        })
    });
}

#[test]
fn the_emulator_credential_needs_no_policy() {
    on_enforcing_router(|router| {
        Box::pin(async move {
            let (status, body) = describe_db_instances(&router, ADMINISTRATOR_KEY).await;

            assert_eq!(status, 200, "{body}");
        })
    });
}
