// SPDX-License-Identifier: CC0-1.0

//! What the router suites share: the router the binary serves, with request authentication
//! and IAM enforcement on, and requests signed as SDKs sign them with the keys the emulator
//! vends.

#![allow(dead_code)]

use aws_emulator_fork_router::{build_router, InMemoryEngine};
use aws_sigv4::http_request::{sign, SignableBody, SignableRequest, SigningParams, SigningSettings};
use bytes::Bytes;
use emulator_server::{EmulatorService, SigV4Authenticator};
use std::sync::Arc;
use std::time::SystemTime;

/// The emulator endpoint the requests are addressed to.
pub const HOST: &str = "127.0.0.1:9297";

/// Assembling the router needs a larger stack than a test thread has (see main.rs).
const ROUTER_STACK_SIZE: usize = 64 * 1024 * 1024;

pub type Router = emulator_server::WithExtraRoutes<smithy_server_runtime::MultiServiceRouter>;

/// Run `case` against a router with authentication and IAM enforcement on, handing it the
/// engine the router serves from as well.
pub fn on_authenticating_router(
    case: impl FnOnce(Router, Arc<InMemoryEngine>) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> + Send + 'static,
) {
    std::thread::Builder::new()
        .stack_size(ROUTER_STACK_SIZE)
        .spawn(move || {
            // Every case runs with both switches on, so the process-wide writes are the same
            // whichever case runs first.
            emulator_server::set_authentication(true);
            std::env::set_var(aws_emulator_fork_iam_gate::ENFORCE_ENV, "on");
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a tokio runtime");
            let engine = Arc::new(InMemoryEngine::new());
            runtime.block_on(case(build_router(Arc::clone(&engine)), engine));
        })
        .expect("spawn the router thread")
        .join()
        .expect("the case passes");
}

/// The key pair the emulator vends, which `--auth on` verifies requests against.
pub fn emulator_keys() -> (String, String) {
    let keys = SigV4Authenticator::minted_credentials().expect("an authenticating router mints the keys");
    (keys.access_key_id.clone(), keys.secret_access_key.clone())
}

pub enum Scheme {
    SigV4 { signing_name: &'static str, region: &'static str },
    SigV4a { signing_name: &'static str, region_set: &'static str },
}

/// The request, signed as an SDK signs it with `key` and `secret`.
pub fn signed(scheme: &Scheme, keys: (&str, &str), method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> http::Request<Bytes> {
    signed_for(HOST, scheme, keys, method, path, headers, body)
}

/// [`signed`], addressed to `host` rather than [`HOST`].
pub fn signed_for(
    host: &str,
    scheme: &Scheme,
    (key, secret): (&str, &str),
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> http::Request<Bytes> {
    let uri = format!("https://{host}{path}");
    let mut request = http::Request::builder().method(method).uri(&uri).header("host", host);
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

/// [`signed`] with the emulator's own keys.
pub fn signed_by_emulator(scheme: &Scheme, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> http::Request<Bytes> {
    let (key, secret) = emulator_keys();
    signed(scheme, (&key, &secret), method, path, headers, body)
}

pub struct Reply {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub text: String,
}

impl Reply {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.text).unwrap_or_else(|_| panic!("not JSON: {}", self.text))
    }

    pub fn etag(&self) -> String {
        let etag = self.headers.get("etag").unwrap_or_else(|| panic!("no ETag on {} {}", self.status, self.text));
        etag.to_str().expect("a text ETag").to_string()
    }

    /// The error code a refusal names, from `x-amzn-errortype` or the body.
    pub fn error_code(&self) -> String {
        let header = self.headers.get("x-amzn-errortype").and_then(|value| value.to_str().ok());
        header.map(str::to_string).unwrap_or_else(|| self.text.clone())
    }

    pub fn header(&self, name: &str) -> String {
        self.headers.get(name).and_then(|value| value.to_str().ok()).unwrap_or_default().to_string()
    }
}

pub async fn send(router: &Router, request: http::Request<Bytes>) -> Reply {
    let response = router.handle_request(request).await;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let text = String::from_utf8_lossy(response.body()).into_owned();
    Reply { status, headers, text }
}

/// A store ARN or key as one path segment, as SDKs encode an `@httpLabel`.
pub fn segment(raw: &str) -> String {
    let encode = |byte: u8| match byte {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => char::from(byte).to_string(),
        _ => format!("%{byte:02X}"),
    };
    raw.bytes().map(encode).collect()
}

/// The text of the first `<name>` element of `xml`.
pub fn element(xml: &str, name: &str) -> String {
    let after = xml.split(&format!("<{name}>")).nth(1).unwrap_or_else(|| panic!("no <{name}> in {xml}"));
    after.split(&format!("</{name}>")).next().expect("a closing tag").to_string()
}

/// The CloudFront control plane, which SDKs sign with SigV4 for `us-east-1`.
pub const CLOUDFRONT: Scheme = Scheme::SigV4 { signing_name: "cloudfront", region: "us-east-1" };

/// The key value store data plane, which SDKs sign with SigV4A.
pub const KEY_VALUE_STORE_DATA_PLANE: Scheme = Scheme::SigV4a { signing_name: "cloudfront-keyvaluestore", region_set: "*" };

/// `CreateKeyValueStore` on the CloudFront control plane; answers the store's ARN.
pub async fn create_key_value_store(router: &Router, name: &str) -> String {
    let body = format!(
        "<CreateKeyValueStoreRequest xmlns=\"http://cloudfront.amazonaws.com/doc/2020-05-31/\">\
         <Name>{name}</Name><Comment>host to cell routes</Comment></CreateKeyValueStoreRequest>"
    );
    let headers = [("content-type", "application/xml")];
    let request = signed_by_emulator(&CLOUDFRONT, "POST", "/2020-05-31/key-value-store", &headers, &body);
    let created = send(router, request).await;
    assert_eq!(created.status, 201, "CreateKeyValueStore: {}", created.text);
    element(&created.text, "ARN")
}
