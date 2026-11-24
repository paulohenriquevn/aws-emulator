// SPDX-License-Identifier: CC0-1.0

//! A CloudFront key value store is usable end to end through the router the binary serves,
//! with request authentication on (`--auth on`) and IAM enforcement on, as a deployment runs
//! it: CloudFront `CreateKeyValueStore` creates the store, and its data plane
//! (`cloudfront-keyvaluestore`) describes it and puts, reads and lists keys.
//!
//! SDKs sign the data plane with SigV4A (`AWS4-ECDSA-P256-SHA256`), whose credential scope has
//! no region and whose region set travels in `X-Amz-Region-Set`. These requests are signed with
//! aws-sigv4's own SigV4A signer, the one the Rust SDK uses, and the control plane call with its
//! SigV4 signer, with the keys the emulator vends.

use aws_emulator_fork_router::{build_router, InMemoryEngine};
use aws_sigv4::http_request::{sign, SignableBody, SignableRequest, SigningParams, SigningSettings};
use bytes::Bytes;
use emulator_server::{EmulatorService, SigV4Authenticator};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::SystemTime;

const HOST: &str = "127.0.0.1:9297";
const DATA_PLANE: &str = "cloudfront-keyvaluestore";
const ROUTE_KEY: &str = "tenant-a.example.com";

/// Assembling the router needs a larger stack than a test thread has (see main.rs).
const ROUTER_STACK_SIZE: usize = 64 * 1024 * 1024;

type Router = emulator_server::WithExtraRoutes<smithy_server_runtime::MultiServiceRouter>;

fn on_authenticating_router(
    case: impl FnOnce(Router) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> + Send + 'static,
) {
    std::thread::Builder::new()
        .stack_size(ROUTER_STACK_SIZE)
        .spawn(move || {
            // Every case of this binary runs with both switches on, so the process-wide writes
            // are the same whichever case runs first.
            emulator_server::set_authentication(true);
            std::env::set_var(aws_emulator_fork_iam_gate::ENFORCE_ENV, "on");
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a tokio runtime");
            runtime.block_on(case(build_router(Arc::new(InMemoryEngine::new()))));
        })
        .expect("spawn the router thread")
        .join()
        .expect("the case passes");
}

/// The key pair the emulator vends, which `--auth on` verifies requests against.
fn emulator_keys() -> (String, String) {
    let keys = SigV4Authenticator::minted_credentials().expect("an authenticating router mints the keys");
    (keys.access_key_id.clone(), keys.secret_access_key.clone())
}

enum Scheme {
    SigV4 { signing_name: &'static str, region: &'static str },
    SigV4a { signing_name: &'static str, region_set: &'static str },
}

/// The request, signed as an SDK signs it with `key` and `secret`.
fn signed(scheme: &Scheme, (key, secret): (&str, &str), method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> http::Request<Bytes> {
    let uri = format!("https://{HOST}{path}");
    let mut request = http::Request::builder().method(method).uri(&uri).header("host", HOST);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = request.body(Bytes::from(body.to_string())).expect("a well-formed request");
    let identity = aws_credential_types::Credentials::new(key, secret, None, None, "test").into();
    let (settings, now) = (SigningSettings::default(), SystemTime::now());
    let params: SigningParams<'_> = match scheme {
        Scheme::SigV4 { signing_name, region } => {
            let params = aws_sigv4::sign::v4::SigningParams::builder().identity(&identity).region(region);
            params.name(signing_name).time(now).settings(settings).build().expect("SigV4 parameters").into()
        }
        Scheme::SigV4a { signing_name, region_set } => {
            let params = aws_sigv4::sign::v4a::SigningParams::builder().identity(&identity).region_set(region_set);
            params.name(signing_name).time(now).settings(settings).build().expect("SigV4A parameters").into()
        }
    };
    let signable_headers = request.headers().iter().map(|(name, value)| (name.as_str(), value.to_str().expect("text header")));
    let signable = SignableRequest::new(method, &uri, signable_headers, SignableBody::Bytes(body.as_bytes())).expect("a signable request");
    let (instructions, _) = sign(signable, &params).expect("the request signs").into_parts();
    instructions.apply_to_request_http1x(&mut request);
    request
}

struct Reply {
    status: u16,
    headers: http::HeaderMap,
    text: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|_| panic!("not JSON: {}", self.text))
    }

    fn etag(&self) -> String {
        let etag = self.headers.get("etag").unwrap_or_else(|| panic!("no ETag on {} {}", self.status, self.text));
        etag.to_str().expect("a text ETag").to_string()
    }

    /// The error code a refusal names, from `x-amzn-errortype` or the body.
    fn error_code(&self) -> String {
        let header = self.headers.get("x-amzn-errortype").and_then(|value| value.to_str().ok());
        header.map(str::to_string).unwrap_or_else(|| self.text.clone())
    }
}

async fn send(router: &Router, request: http::Request<Bytes>) -> Reply {
    let response = router.handle_request(request).await;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let text = String::from_utf8_lossy(response.body()).into_owned();
    Reply { status, headers, text }
}

/// A store ARN or key as one path segment, as SDKs encode an `@httpLabel`.
fn segment(raw: &str) -> String {
    let encode = |byte: u8| match byte {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => char::from(byte).to_string(),
        _ => format!("%{byte:02X}"),
    };
    raw.bytes().map(encode).collect()
}

fn element(xml: &str, name: &str) -> String {
    let after = xml.split(&format!("<{name}>")).nth(1).unwrap_or_else(|| panic!("no <{name}> in {xml}"));
    after.split(&format!("</{name}>")).next().expect("a closing tag").to_string()
}

const DATA_PLANE_SIGV4A: Scheme = Scheme::SigV4a { signing_name: DATA_PLANE, region_set: "*" };

/// `CreateKeyValueStore` on the CloudFront control plane, which SDKs sign with SigV4; answers
/// the store's ARN.
async fn create_key_value_store(router: &Router, name: &str) -> String {
    let (key, secret) = emulator_keys();
    let body = format!(
        "<CreateKeyValueStoreRequest xmlns=\"http://cloudfront.amazonaws.com/doc/2020-05-31/\">\
         <Name>{name}</Name><Comment>host to cell routes</Comment></CreateKeyValueStoreRequest>"
    );
    let scheme = Scheme::SigV4 { signing_name: "cloudfront", region: "us-east-1" };
    let headers = [("content-type", "application/xml")];
    let request = signed(&scheme, (&key, &secret), "POST", "/2020-05-31/key-value-store", &headers, &body);
    let created = send(router, request).await;
    assert_eq!(created.status, 201, "CreateKeyValueStore: {}", created.text);
    element(&created.text, "ARN")
}

async fn data_plane(router: &Router, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let (key, secret) = emulator_keys();
    let mut all_headers = vec![("content-type", "application/json")];
    all_headers.extend_from_slice(headers);
    send(router, signed(&DATA_PLANE_SIGV4A, (&key, &secret), method, path, &all_headers, body)).await
}

#[test]
fn a_store_created_on_cloudfront_is_read_and_written_over_sigv4a() {
    on_authenticating_router(|router| {
        Box::pin(async move {
            let arn = create_key_value_store(&router, "host-to-cell-routes").await;
            assert!(arn.starts_with("arn:aws:cloudfront::"), "{arn}");
            let store = format!("/key-value-stores/{}", segment(&arn));

            let described = data_plane(&router, "GET", &store, &[], "").await;
            assert_eq!(described.status, 200, "DescribeKeyValueStore: {}", described.text);
            assert_eq!(described.json()["KvsARN"], json!(arn));
            assert_eq!(described.json()["ItemCount"], json!(0));
            let first_etag = described.etag();

            let key_path = format!("{store}/keys/{}", segment(ROUTE_KEY));
            let value = json!({ "Value": "cell-1" }).to_string();
            let put = data_plane(&router, "PUT", &key_path, &[("if-match", &first_etag)], &value).await;
            assert_eq!(put.status, 200, "PutKey: {}", put.text);
            assert_eq!(put.json()["ItemCount"], json!(1));
            let second_etag = put.etag();
            assert_ne!(second_etag, first_etag, "a put moves the store to a new version");

            let moved = json!({ "Value": "cell-2" }).to_string();
            let stale = data_plane(&router, "PUT", &key_path, &[("if-match", &first_etag)], &moved).await;
            assert_eq!(stale.status, 409, "PutKey with a stale If-Match: {}", stale.text);
            assert!(stale.error_code().contains("ConflictException"), "{}", stale.error_code());

            let read = data_plane(&router, "GET", &key_path, &[], "").await;
            assert_eq!(read.status, 200, "GetKey: {}", read.text);
            assert_eq!(read.json()["Key"], json!(ROUTE_KEY));
            assert_eq!(read.json()["Value"], json!("cell-1"), "the stale put changed nothing");

            let current = data_plane(&router, "PUT", &key_path, &[("if-match", &second_etag)], &moved).await;
            assert_eq!(current.status, 200, "PutKey with the current If-Match: {}", current.text);

            let listed = data_plane(&router, "GET", &format!("{store}/keys?MaxResults=10"), &[], "").await;
            assert_eq!(listed.status, 200, "ListKeys: {}", listed.text);
            assert_eq!(listed.json()["Items"], json!([{ "Key": ROUTE_KEY, "Value": "cell-2" }]));
        })
    });
}

#[test]
fn a_data_plane_call_not_signed_with_the_emulator_keys_is_refused() {
    on_authenticating_router(|router| {
        Box::pin(async move {
            let arn = create_key_value_store(&router, "refusals").await;
            let path = format!("/key-value-stores/{}", segment(&arn));
            let (key, secret) = emulator_keys();
            let headers = [("content-type", "application/json")];

            let wrong_secret = signed(&DATA_PLANE_SIGV4A, (&key, "not-the-secret"), "GET", &path, &headers, "");
            let refused = send(&router, wrong_secret).await;
            assert_eq!(refused.status, 403, "{}", refused.text);
            assert!(refused.error_code().contains("SignatureDoesNotMatch"), "{}", refused.error_code());

            let other_service = Scheme::SigV4a { signing_name: "cloudfront", region_set: "*" };
            let misscoped = send(&router, signed(&other_service, (&key, &secret), "GET", &path, &headers, "")).await;
            assert_eq!(misscoped.status, 403, "{}", misscoped.text);
            assert!(misscoped.error_code().contains("AccessDenied"), "{}", misscoped.error_code());

            let mut tampered = signed(&DATA_PLANE_SIGV4A, (&key, &secret), "GET", &path, &headers, "");
            *tampered.uri_mut() = format!("https://{HOST}{path}/keys/{}", segment(ROUTE_KEY)).parse().expect("a URI");
            let retargeted = send(&router, tampered).await;
            assert_eq!(retargeted.status, 403, "{}", retargeted.text);
            assert!(retargeted.error_code().contains("SignatureDoesNotMatch"), "{}", retargeted.error_code());
        })
    });
}
