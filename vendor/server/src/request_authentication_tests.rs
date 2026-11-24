// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
use super::*;
use std::time::{Duration, SystemTime};

const KEY: &str = "EMULATORAAAAAAAAAAAA";
const SECRET: &str = "emulator-test-secret";

fn emulator() -> SigV4Authenticator {
    SigV4Authenticator {
        access_key_id: KEY.into(),
        secret_access_key: SECRET.into(),
    }
}

fn store() -> CredentialStore {
    CredentialStore::default()
}

/// How a test client signs: the SDK signer, as an SDK would.
#[derive(Clone, Copy)]
struct Client {
    service: &'static str,
    region: &'static str,
    hashed_payload: bool,
    key: &'static str,
    secret: &'static str,
    presigned_for: Option<Duration>,
    signed_at: SystemTime,
    session_token: Option<&'static str>,
}

const SQS: Client = Client {
    service: "sqs",
    region: "us-west-2",
    hashed_payload: false,
    key: KEY,
    secret: SECRET,
    presigned_for: None,
    signed_at: SystemTime::UNIX_EPOCH,
    session_token: None,
};

/// The SDKs sign IoT Secure Tunneling with its modeled `aws.auth#sigv4` name as written.
const IOT_SECURE_TUNNELING: Client = Client {
    service: "IoTSecuredTunneling",
    ..SQS
};

fn request(method: &str, uri: &str, body: &[u8]) -> http::Request<Vec<u8>> {
    let builder = http::Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1:8443");
    builder
        .header("content-type", "application/x-amz-json-1.0")
        .body(body.to_vec())
        .unwrap()
}

fn signed(client: &Client, mut request: http::Request<Vec<u8>>) -> (http::request::Parts, Vec<u8>) {
    let credentials =
        aws_credential_types::Credentials::new(client.key, client.secret, None, None, "test");
    let identity = credentials.into();
    let mut settings = SigningSettings::default();
    if signs_like_s3(client.service) {
        settings.percent_encoding_mode = PercentEncodingMode::Single;
        settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
    }
    if let Some(expires) = client.presigned_for {
        settings.signature_location = SignatureLocation::QueryParams;
        settings.expires_in = Some(expires);
    }
    let unset = client.signed_at == SystemTime::UNIX_EPOCH;
    let at = if unset {
        SystemTime::now()
    } else {
        client.signed_at
    };
    let s3_header_signed = client.presigned_for.is_none() && signs_like_s3(client.service);
    if s3_header_signed {
        settings.payload_checksum_kind = aws_sigv4::http_request::PayloadChecksumKind::XAmzSha256;
    }
    let body = match (client.presigned_for, s3_header_signed) {
        (None, true) if !client.hashed_payload => SignableBody::UnsignedPayload,
        (Some(_), _) => SignableBody::UnsignedPayload,
        _ => SignableBody::Bytes(request.body()),
    };
    let params = aws_sigv4::sign::v4::SigningParams::builder().identity(&identity);
    let params = params.region(client.region).name(client.service);
    let params = params.time(at).settings(settings).build().unwrap().into();
    let (method, uri) = (request.method().to_string(), request.uri().to_string());
    let headers = request.headers().iter();
    let headers = headers.map(|(name, value)| (name.as_str(), value.to_str().unwrap()));
    let signable = SignableRequest::new(&method, &uri, headers, body).unwrap();
    let (instructions, _) = sign(signable, &params).unwrap().into_parts();
    instructions.apply_to_request_http1x(&mut request);
    request.into_parts()
}

fn send_message() -> http::Request<Vec<u8>> {
    request("POST", "https://127.0.0.1:8443/", br#"{"QueueUrl":"q"}"#)
}

fn in_process() -> http::Request<Vec<u8>> {
    http::Request::builder()
        .method("POST")
        .uri("https://127.0.0.1:8443/")
        .body(vec![])
        .unwrap()
}

fn get_object() -> http::Request<Vec<u8>> {
    request("GET", "https://127.0.0.1:8443/bucket/key%20one", b"")
}

fn verdict(parts: &http::request::Parts, body: &[u8], serving: &str) -> Result<(), &'static str> {
    verdict_in(&store(), parts, body, serving)
}

fn verdict_in(
    store: &CredentialStore,
    parts: &http::request::Parts,
    body: &[u8],
    serving: &str,
) -> Result<(), &'static str> {
    let verified = emulator().verify(parts, body, serving, store);
    verified.map_err(|denied| denied.code)
}

#[test]
fn created_keys_verify_until_deleted_and_the_published_key_always() {
    let store = store();
    let created = store.create(&crate::aws_credentials::AWS_ACCESS_KEY);
    let leak = |text: &str| -> &'static str { Box::leak(text.into()) };
    let secret = leak(created.secret(SECRET_ACCESS_KEY).unwrap());
    let as_created = Client { key: leak(&created.id), secret, ..SQS };
    let sent = br#"{"QueueUrl":"q"}"#.to_vec();
    let verdict_of = |client: &Client| verdict_in(&store, &signed(client, send_message()).0, &sent, "sqs");
    assert_eq!(verdict_of(&as_created), Ok(()));
    assert_eq!(verdict_of(&Client { secret: "x", ..as_created }), Err("SignatureDoesNotMatch"));
    store.delete(&created.id).unwrap();
    assert_eq!(verdict_of(&as_created), Err("AccessDenied"));
    let refused = store.delete(KEY);
    assert_eq!(refused, Err(crate::credential_store::CredentialError::NotFound));
    assert_eq!(verdict_of(&SQS), Ok(()));
}

#[test]
fn requests_are_accepted_or_refused_as_their_signature_says() {
    #[rustfmt::skip]
    let (s3, s3_presigned) = (Client { service: "s3", ..SQS }, Client { service: "s3", presigned_for: Some(Duration::from_secs(600)), ..SQS });
    let hour_ago = SystemTime::now() - Duration::from_secs(3600);
    #[rustfmt::skip]
    let expired = Client { signed_at: hour_ago, presigned_for: Some(Duration::from_secs(60)), ..s3_presigned };
    #[rustfmt::skip]
    let (wrong_secret, hashed) = (Client { secret: "x", ..SQS }, Client { hashed_payload: true, ..s3 });
    let s3_put = || request("PUT", "https://127.0.0.1:8443/bucket/key", b"object");
    let sent = br#"{"QueueUrl":"q"}"#.to_vec();
    let wrong_secret_s3 = signed(&Client { secret: "x", ..s3 }, s3_put()).0;
    let trailer_marked = with_header(
        wrong_secret_s3,
        "x-amz-content-sha256",
        "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
    );
    #[rustfmt::skip]
    let cases: Vec<(&str, http::request::Parts, Vec<u8>, &str, Result<(), &str>)> = vec![
        ("header-signed sqs", signed(&SQS, send_message()).0, sent.clone(), "sqs", Ok(())),
        ("signed in another region", signed(&Client { region: "eu-west-1", ..SQS }, send_message()).0, sent.clone(), "sqs", Ok(())),
        ("presigned s3 get", signed(&s3_presigned, get_object()).0, vec![], "s3", Ok(())),
        ("s3 put with unsigned payload", signed(&s3, s3_put()).0, b"object".to_vec(), "s3", Ok(())),
        ("s3 put with hashed payload", signed(&hashed, s3_put()).0, b"object".to_vec(), "s3", Ok(())),
        ("s3 put, body swapped under its hash", signed(&hashed, s3_put()).0, b"forged".to_vec(), "s3", Err("SignatureDoesNotMatch")),
        ("in-process, no host header", signed(&SQS, in_process()).0, vec![], "sqs", Ok(())),
        ("added unsigned header", with_header(signed(&SQS, send_message()).0, "x-amz-target", "AWSLambda.CreateFunction"), sent.clone(), "sqs", Ok(())),
        ("unsigned", send_message().into_parts().0, vec![], "sqs", Err("AccessDenied")),
        ("another key", signed(&Client { key: "EMULATORB", ..SQS }, send_message()).0, vec![], "sqs", Err("AccessDenied")),
        ("signed for another service", signed(&SQS, send_message()).0, vec![], "lambda", Err("AccessDenied")),
        ("shared s3 url sent to another service", signed(&s3_presigned, get_object()).0, vec![], "lambda", Err("AccessDenied")),
        ("expired presigned url", signed(&expired, get_object()).0, vec![], "s3", Err("AccessDenied")),
        ("presigned expiry past the clock's range", with_uri(signed(&s3_presigned, get_object()).0, "Expires=600", &format!("Expires={}", u64::MAX)), vec![], "s3", Err("AccessDenied")),
        ("wrong secret", signed(&wrong_secret, send_message()).0, sent.clone(), "sqs", Err("SignatureDoesNotMatch")),
        ("wrong secret, unsigned-trailer marker (MinIO CVE-2026-41145)", trailer_marked, vec![], "s3", Err("SignatureDoesNotMatch")),
        ("tampered body", signed(&SQS, send_message()).0, br#"{"QueueUrl":"x"}"#.to_vec(), "sqs", Err("SignatureDoesNotMatch")),
        ("tampered signed header", with_header(signed(&SQS, send_message()).0, "content-type", "application/x-amz-json-1.1"), sent.clone(), "sqs", Err("SignatureDoesNotMatch")),
        ("presigned validity over seven days", with_uri(signed(&s3_presigned, get_object()).0, "Expires=600", "Expires=604801"), vec![], "s3", Err("AccessDenied")),
        ("presigned scope with four parts", with_uri(signed(&s3_presigned, get_object()).0, "%2Faws4_request", ""), vec![], "s3", Err("AccessDenied")),
        ("header-signed ten minutes ago", signed(&Client { signed_at: SystemTime::now() - Duration::from_secs(600), ..SQS }, send_message()).0, sent.clone(), "sqs", Ok(())),
        ("header-signed an hour ago, replayed", signed(&Client { signed_at: hour_ago, ..SQS }, send_message()).0, sent.clone(), "sqs", Err("RequestTimeTooSkewed")),
        ("header-signed by a clock an hour fast", signed(&Client { signed_at: SystemTime::now() + Duration::from_secs(3600), ..SQS }, send_message()).0, sent.clone(), "sqs", Err("RequestTimeTooSkewed")),
        ("presigned url retargeted", with_uri(signed(&s3_presigned, get_object()).0, "/key%20one?", "/other?"), vec![], "s3", Err("SignatureDoesNotMatch")),
        ("mixed-case signing name, served lowercased", signed(&IOT_SECURE_TUNNELING, send_message()).0, sent.clone(), "iotsecuredtunneling", Ok(())),
        ("mixed-case signing name sent to another service", signed(&IOT_SECURE_TUNNELING, send_message()).0, sent.clone(), "iot", Err("AccessDenied")),
        ("mixed-case signing name, tampered body", signed(&IOT_SECURE_TUNNELING, send_message()).0, br#"{"QueueUrl":"x"}"#.to_vec(), "iotsecuredtunneling", Err("SignatureDoesNotMatch")),
        ("ec2 call claimed by a service serving another name", signed(&Client { service: "ec2", ..SQS }, send_message()).0, sent.clone(), "rds", Err("AccessDenied")),
        ("s3 key a//b sent as a/%2Fb, signed decoded (Java v2)", with_uri(signed(&s3, s3_get("a//b")).0, "/a//b", "/a/%2Fb"), vec![], "s3", Ok(())),
        ("s3express-signed call served by an s3 service (s3control)", signed(&Client { service: "s3express", ..s3 }, s3_get("k")).0, vec![], "s3", Ok(())),
        ("s3express-signed call sent to a non-S3 service", signed(&Client { service: "s3express", ..s3 }, s3_get("k")).0, vec![], "sqs", Err("AccessDenied")),
        ("s3-outposts key a//b sent as a/%2Fb, signed decoded", with_uri(signed(&Client { service: "s3-outposts", ..s3 }, s3_get("a//b")).0, "/a//b", "/a/%2Fb"), vec![], "s3-outposts", Ok(())),
        ("s3 key holding a literal %2F (boto3)", signed(&s3, s3_get("a%252Fb")).0, vec![], "s3", Ok(())),
        ("s3 key with ! + space ~ and UTF-8, encoded as SDKs send it", signed(&s3, s3_get("%21%2B%20~%D0%BA")).0, vec![], "s3", Ok(())),
        ("host left out of SignedHeaders", without_signed_host(signed(&SQS, send_message()).0), sent.clone(), "sqs", Err("SignatureDoesNotMatch")),
    ];
    for (name, parts, body, serving, expected) in cases {
        assert_eq!(verdict(&parts, &body, serving), expected, "{name}");
    }
}

fn with_header(
    mut parts: http::request::Parts,
    name: &'static str,
    value: &str,
) -> http::request::Parts {
    parts.headers.insert(name, value.parse().unwrap());
    parts
}

fn s3_get(key: &str) -> http::Request<Vec<u8>> {
    request("GET", &format!("https://127.0.0.1:8443/bucket/{key}"), b"")
}

fn without_signed_host(parts: http::request::Parts) -> http::request::Parts {
    let authorization = parts.headers["authorization"]
        .to_str()
        .unwrap()
        .replace(";host", "");
    with_header(parts, "authorization", &authorization)
}

fn with_uri(mut parts: http::request::Parts, from: &str, to: &str) -> http::request::Parts {
    parts.uri = parts.uri.to_string().replace(from, to).parse().unwrap();
    parts
}

#[test]
fn the_switch_is_off_by_default_and_the_credentials_file_is_private() {
    let home = std::env::temp_dir().join(format!("request-authentication-{}", std::process::id()));
    // SAFETY: no other test in this binary reads or writes these variables.
    unsafe { std::env::set_var("HOME", &home) };
    assert!(!authentication_enabled(), "off by default");
    for enabled in [true, false] {
        set_authentication(enabled);
        assert_eq!(authentication_enabled(), enabled);
    }
    let written = write_credentials_file(&emulator(), 4566);
    #[cfg(not(unix))]
    assert_eq!(written.unwrap_err().kind(), std::io::ErrorKind::Unsupported);
    #[cfg(unix)]
    {
        let file = written.unwrap();
        assert_eq!(file.0, home.join(".emulator/4566/emulator_credentials"));
        assert!(std::fs::read_to_string(&file.0).unwrap().contains(KEY));
        let mode = std::fs::metadata(&file.0).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
        let path = file.0.clone();
        drop(file);
        assert!(!path.exists());
        let planted_target = home.join("planted");
        std::fs::write(&planted_target, "untouched").unwrap();
        std::os::unix::fs::symlink(&planted_target, &path).unwrap();
        let rewritten = write_credentials_file(&emulator(), 4566).unwrap();
        assert_eq!(
            std::fs::read_to_string(&planted_target).unwrap(),
            "untouched"
        );
        assert!(
            !std::fs::symlink_metadata(&rewritten.0)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let ca = write_emulator_file(4566, "ca.pem", "pem").unwrap();
        assert_eq!(std::fs::read_to_string(&ca.0).unwrap(), "pem");
        assert_eq!(ca.0, home.join(".emulator/4566/ca.pem"));
        created_secrets_stay_off_disk(&home.join(".emulator/4566"));
    }
}

/// Creates credentials, then finds only the emulator's own files in its port folder, none with
/// a created secret.
fn created_secrets_stay_off_disk(port_folder: &std::path::Path) {
    let created = store().create(&crate::aws_credentials::AWS_SESSION);
    let files: Vec<_> = std::fs::read_dir(port_folder).unwrap().map(|e| e.unwrap().path()).collect();
    let names: Vec<_> = files.iter().map(|path| path.file_name().unwrap()).collect();
    assert_eq!(names.len(), 2, "{files:?}");
    assert!(names.contains(&"emulator_credentials".as_ref()) && names.contains(&"ca.pem".as_ref()));
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        assert!(created.secrets.values().all(|secret| !text.contains(secret.as_str())), "{file:?}");
    }
}

#[test]
fn vended_credentials_debug_redacts_the_secret() {
    let vended = crate::VendedCredentials::AwsAccessKey {
        access_key_id: "EMULATORKEY".to_string(),
        secret_access_key: "s3cr3t".to_string(),
    };
    let printed = format!("{vended:?}");
    assert!(
        printed.contains("EMULATORKEY") && !printed.contains("s3cr3t"),
        "{printed}"
    );
}

/// What a later caller sends for a URL `presign_url` signed: the URL's path and query, with the
/// `Host` it connects to and any headers the signer bound.
fn fetched(url: &str, method: &str, headers: &[(&str, &str)]) -> http::request::Parts {
    let rest = url.strip_prefix("https://127.0.0.1:8443").expect("the endpoint is kept");
    let mut builder = http::Request::builder().method(method).uri(rest);
    builder = builder.header("host", "127.0.0.1:8443");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(()).unwrap().into_parts().0
}

fn presigned(
    signing_name: &str,
    path: &str,
    query: &[(&str, &str)],
    headers: &[(&str, &str)],
    signed_at: SystemTime,
) -> String {
    let target = PresignTarget {
        endpoint: "https://127.0.0.1:8443",
        method: "GET",
        signing_name,
        region: "us-west-2",
        path,
        query,
        headers,
    };
    presign_url(&target, KEY, SECRET, signed_at, 600).expect("signable")
}

#[test]
fn presigned_s3_keys_with_spaces_plus_and_encoded_slashes_verify() {
    for key in ["/b/with space", "/b/a+b", "/b/a%2Fb", "/b/dir/k~1.txt", "/b/naïve"] {
        let url = presigned("s3", key, &[("response-content-type", "a/b+c")], &[], SystemTime::now());
        let parts = fetched(&url, "GET", &[]);
        let decoded = percent_encoding::percent_decode_str(parts.uri.path()).decode_utf8().unwrap();
        assert_eq!(decoded, key, "the path decodes back to the key");
        assert_eq!(emulator().verify(&parts, b"", "s3", &store()), Ok(()), "{key}: {url}");
    }
}

#[test]
fn presigned_urls_for_other_signing_names_verify_with_an_empty_body() {
    let url = presigned("lambda", "/2015-03-31/functions/f", &[], &[], SystemTime::now());
    let parts = fetched(&url, "GET", &[]);
    assert_eq!(emulator().verify(&parts, b"", "lambda", &store()), Ok(()), "{url}");
    assert_eq!(emulator().verify(&parts, b"x", "lambda", &store()), Err(SIGNATURE_MISMATCH));
}

#[test]
fn a_presigned_put_with_a_body_verifies_for_any_signing_name_with_the_payload_header() {
    for (signing_name, path) in [("lambda", "/2015-03-31/functions/f"), ("s3", "/b/k")] {
        let target = PresignTarget {
            endpoint: "https://127.0.0.1:8443",
            method: "PUT",
            signing_name,
            region: "us-west-2",
            path,
            query: &[],
            headers: &[("content-type", "application/json")],
        };
        let url = presign_url(&target, KEY, SECRET, SystemTime::now(), 600).expect("signable");
        let mut sent = vec![("content-type", "application/json")];
        sent.extend(presigned_payload_header(signing_name, "PUT"));
        let parts = fetched(&url, "PUT", &sent);
        let body = br#"{"Code":"any body"}"#;
        assert_eq!(emulator().verify(&parts, body, signing_name, &store()), Ok(()), "{url}");
    }
    assert_eq!(
        presigned_payload_header("lambda", "PUT"),
        Some(("x-amz-content-sha256", "UNSIGNED-PAYLOAD"))
    );
    assert_eq!(presigned_payload_header("lambda", "GET"), None);
    assert_eq!(presigned_payload_header("s3", "PUT"), None);
}

#[test]
fn a_presigned_put_sent_without_its_payload_header_is_refused() {
    let target = PresignTarget {
        endpoint: "https://127.0.0.1:8443",
        method: "PUT",
        signing_name: "lambda",
        region: "us-west-2",
        path: "/2015-03-31/functions/f",
        query: &[],
        headers: &[],
    };
    let url = presign_url(&target, KEY, SECRET, SystemTime::now(), 600).expect("signable");
    let parts = fetched(&url, "PUT", &[]);
    assert_eq!(emulator().verify(&parts, b"body", "lambda", &store()), Err(SIGNATURE_MISMATCH));
}

#[test]
fn a_header_the_signer_bound_must_be_sent_as_signed() {
    let bound = [("content-type", "image/png")];
    let url = presigned("s3", "/b/k", &[], &bound, SystemTime::now());
    let sent = fetched(&url, "GET", &bound);
    assert_eq!(emulator().verify(&sent, b"", "s3", &store()), Ok(()));
    let changed = fetched(&url, "GET", &[("content-type", "text/html")]);
    assert_eq!(emulator().verify(&changed, b"", "s3", &store()), Err(SIGNATURE_MISMATCH));
}

#[test]
fn a_presigned_url_is_refused_once_tampered_or_expired() {
    let url = presigned("s3", "/b/k", &[], &[], SystemTime::now());
    let tampered = fetched(&url.replace("/b/k?", "/b/other?"), "GET", &[]);
    assert_eq!(emulator().verify(&tampered, b"", "s3", &store()), Err(SIGNATURE_MISMATCH));
    let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
    let expired = fetched(&presigned("s3", "/b/k", &[], &[], an_hour_ago), "GET", &[]);
    assert_eq!(emulator().verify(&expired, b"", "s3", &store()), Err(ACCESS_DENIED));
}

#[test]
fn a_target_without_a_rooted_path_is_not_signed() {
    let target = PresignTarget {
        endpoint: "https://127.0.0.1:8443",
        method: "GET",
        signing_name: "s3",
        region: "us-west-2",
        path: "b/k",
        query: &[],
        headers: &[],
    };
    assert_eq!(presign_url(&target, KEY, SECRET, SystemTime::now(), 600), None);
}

/// The verifier refuses a validity of more than seven days, so the signer does not mint one.
#[test]
fn a_validity_the_verifier_refuses_is_not_signed() {
    let target = PresignTarget {
        endpoint: "https://127.0.0.1:8443",
        method: "GET",
        signing_name: "s3",
        region: "us-west-2",
        path: "/b/k",
        query: &[],
        headers: &[],
    };
    let now = SystemTime::now();
    assert!(presign_url(&target, KEY, SECRET, now, MAX_PRESIGNED_EXPIRES_SECS).is_some());
    assert_eq!(presign_url(&target, KEY, SECRET, now, MAX_PRESIGNED_EXPIRES_SECS + 1), None);
    assert_eq!(presign_url(&target, KEY, SECRET, now, 0), None);
}

// fork: SigV4A (`AWS4-ECDSA-P256-SHA256`), the scheme SDKs sign the CloudFront KeyValueStore
// data plane with.

/// The data plane of a CloudFront key value store, as SDKs sign it: SigV4A over the region set
/// `*`, under the signing name `cloudfront-keyvaluestore`.
const KVS: Client = Client {
    service: "cloudfront-keyvaluestore",
    region: "*",
    ..SQS
};

/// A store ARN in a path, as SDKs encode it: `:` and `/` percent-encoded.
const STORE_PATH: &str = "/key-value-stores/arn%3Aaws%3Acloudfront%3A%3A000000000000%3A\
                          key-value-store%2F0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";
const PUT_KEY_BODY: &[u8] = br#"{"Value":"cell-1"}"#;

/// Signs as an SDK signs a SigV4A request: aws-sigv4's own SigV4A signer, for the region set
/// `client.region` names.
fn sigv4a_signed(
    client: &Client,
    mut request: http::Request<Vec<u8>>,
) -> (http::request::Parts, Vec<u8>) {
    let credentials = aws_credential_types::Credentials::new(
        client.key,
        client.secret,
        client.session_token.map(str::to_string),
        None,
        "test",
    );
    let identity = credentials.into();
    let mut settings = SigningSettings::default();
    if signs_like_s3(client.service) {
        settings.percent_encoding_mode = PercentEncodingMode::Single;
        settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
    }
    let unset = client.signed_at == SystemTime::UNIX_EPOCH;
    let at = if unset {
        SystemTime::now()
    } else {
        client.signed_at
    };
    let params = aws_sigv4::sign::v4a::SigningParams::builder().identity(&identity);
    let params = params.region_set(client.region).name(client.service);
    let params = params.time(at).settings(settings).build().unwrap().into();
    let (method, uri) = (request.method().to_string(), request.uri().to_string());
    let headers = request.headers().iter();
    let headers = headers.map(|(name, value)| (name.as_str(), value.to_str().unwrap()));
    let body = SignableBody::Bytes(request.body());
    let signable = SignableRequest::new(&method, &uri, headers, body).unwrap();
    let (instructions, _) = sign(signable, &params).unwrap().into_parts();
    instructions.apply_to_request_http1x(&mut request);
    request.into_parts()
}

fn put_key() -> http::Request<Vec<u8>> {
    let uri = format!("https://127.0.0.1:8443{STORE_PATH}/keys/tenant.example.com");
    let mut request = request("PUT", &uri, PUT_KEY_BODY);
    let etag = http::HeaderValue::from_static("ETAG-1");
    request.headers_mut().insert("if-match", etag);
    request
}

fn list_keys() -> http::Request<Vec<u8>> {
    let uri = format!("https://127.0.0.1:8443{STORE_PATH}/keys?MaxResults=10&NextToken=a%2Bb%3D");
    request("GET", &uri, b"")
}

fn with_authorization(parts: http::request::Parts, from: &str, to: &str) -> http::request::Parts {
    let authorization = parts.headers["authorization"].to_str().unwrap().replace(from, to);
    with_header(parts, "authorization", &authorization)
}

fn without_header(mut parts: http::request::Parts, name: &str) -> http::request::Parts {
    parts.headers.remove(name);
    parts
}

#[test]
fn a_sigv4a_authorization_header_is_recognized() {
    let (parts, _) = sigv4a_signed(&KVS, put_key());
    assert!(emulator().recognizes(&parts));
    let other = with_authorization(parts, "AWS4-ECDSA-P256-SHA256 ", "AWS4-ECDSA-P384-SHA384 ");
    assert!(!emulator().recognizes(&other), "an algorithm this emulator does not verify");
}

#[test]
fn sigv4a_requests_are_accepted_or_refused_as_their_signature_says() {
    let hour_ago = SystemTime::now() - Duration::from_secs(3600);
    let kvs = "cloudfront-keyvaluestore";
    let put = || sigv4a_signed(&KVS, put_key()).0;
    let body = PUT_KEY_BODY.to_vec();
    #[rustfmt::skip]
    let cases: Vec<(&str, http::request::Parts, Vec<u8>, &str, Result<(), &str>)> = vec![
        ("put key", put(), body.clone(), kvs, Ok(())),
        ("list keys, query with encoded + and =", sigv4a_signed(&KVS, list_keys()).0, vec![], kvs, Ok(())),
        ("signed for the region set us-east-1", sigv4a_signed(&Client { region: "us-east-1", ..KVS }, put_key()).0, body.clone(), kvs, Ok(())),
        ("signed for a region set of two", sigv4a_signed(&Client { region: "us-east-1,eu-west-1", ..KVS }, put_key()).0, body.clone(), kvs, Ok(())),
        ("in-process, no host header", sigv4a_signed(&KVS, in_process()).0, vec![], kvs, Ok(())),
        ("s3 multi-region access point get, key with a space", sigv4a_signed(&Client { service: "s3", ..KVS }, s3_get("key%20one")).0, vec![], "s3", Ok(())),
        ("added unsigned header", with_header(put(), "x-amz-user-agent", "other"), body.clone(), kvs, Ok(())),
        ("tampered body", put(), br#"{"Value":"cell-2"}"#.to_vec(), kvs, Err("SignatureDoesNotMatch")),
        ("tampered signed header", with_header(put(), "if-match", "ETAG-2"), body.clone(), kvs, Err("SignatureDoesNotMatch")),
        ("retargeted path", with_uri(put(), "tenant.example.com", "other.example.com"), body.clone(), kvs, Err("SignatureDoesNotMatch")),
        ("tampered query", with_uri(sigv4a_signed(&KVS, list_keys()).0, "MaxResults=10", "MaxResults=50"), vec![], kvs, Err("SignatureDoesNotMatch")),
        ("tampered region set", with_header(put(), "x-amz-region-set", "us-east-1"), body.clone(), kvs, Err("SignatureDoesNotMatch")),
        ("signed by another secret", sigv4a_signed(&Client { secret: "x", ..KVS }, put_key()).0, body.clone(), kvs, Err("SignatureDoesNotMatch")),
        ("signature that is not DER", with_authorization(put(), "Signature=30", "Signature=31"), body.clone(), kvs, Err("SignatureDoesNotMatch")),
        ("signature that is not hex", with_authorization(put(), "Signature=30", "Signature=zz"), body.clone(), kvs, Err("SignatureDoesNotMatch")),
        ("another key", sigv4a_signed(&Client { key: "EMULATORB", ..KVS }, put_key()).0, body.clone(), kvs, Err("AccessDenied")),
        ("signed for another service", put(), body.clone(), "cloudfront", Err("AccessDenied")),
        ("scope naming another service", with_authorization(put(), "/cloudfront-keyvaluestore/", "/cloudfront/"), body.clone(), kvs, Err("AccessDenied")),
        ("scope with a region field (five parts)", with_authorization(put(), "/cloudfront-keyvaluestore/", "/us-east-1/cloudfront-keyvaluestore/"), body.clone(), kvs, Err("AccessDenied")),
        ("scope with another terminator", with_authorization(put(), "/aws4_request", "/aws4_other"), body.clone(), kvs, Err("AccessDenied")),
        ("region set left out of SignedHeaders", with_authorization(put(), ";x-amz-region-set", ""), body.clone(), kvs, Err("AccessDenied")),
        ("region set header missing", without_header(put(), "x-amz-region-set"), body.clone(), kvs, Err("AccessDenied")),
        ("signed header missing", without_header(put(), "if-match"), body.clone(), kvs, Err("SignatureDoesNotMatch")),
        ("signed an hour ago, replayed", sigv4a_signed(&Client { signed_at: hour_ago, ..KVS }, put_key()).0, body.clone(), kvs, Err("RequestTimeTooSkewed")),
        ("unsigned", put_key().into_parts().0, body.clone(), kvs, Err("AccessDenied")),
    ];
    for (name, parts, body, serving, expected) in cases {
        assert_eq!(verdict(&parts, &body, serving), expected, "{name}");
    }
}

#[test]
fn created_session_credentials_verify_over_sigv4a_with_their_token() {
    let store = store();
    let created = store.create(&crate::aws_credentials::AWS_SESSION);
    let leak = |text: &str| -> &'static str { Box::leak(text.into()) };
    let secret = leak(created.secret(SECRET_ACCESS_KEY).unwrap());
    let token = leak(created.secret(SESSION_TOKEN).unwrap());
    let session = Client { key: leak(&created.id), secret, session_token: Some(token), ..KVS };
    let verdict_of = |parts: &http::request::Parts| {
        verdict_in(&store, parts, PUT_KEY_BODY, "cloudfront-keyvaluestore")
    };
    assert_eq!(verdict_of(&sigv4a_signed(&session, put_key()).0), Ok(()));
    let without_token = Client { session_token: None, ..session };
    let refused = verdict_of(&sigv4a_signed(&without_token, put_key()).0);
    assert_eq!(refused, Err("InvalidClientTokenId"));
    store.delete(&created.id).unwrap();
    assert_eq!(verdict_of(&sigv4a_signed(&session, put_key()).0), Err("AccessDenied"));
}

/// ECDSA signatures are randomized: Go and the CRT (the AWS CLI) sign with a fresh nonce, so a
/// request is never verified by signing it again.
#[test]
fn a_randomized_sigv4a_signature_verifies() {
    let (parts, body) = sigv4a_signed(&KVS, put_key());
    let signed = Signature::of(&parts).unwrap();
    let string_to_sign = sigv4a::string_to_sign(&parts, &body, &signed).unwrap();
    let private_key = aws_sigv4::sign::v4a::generate_signing_key(KEY, SECRET);
    let public_key = sigv4a::public_key(KEY, SECRET).unwrap();
    let algorithm = &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1_SIGNING;
    let key_pair = aws_lc_rs::signature::EcdsaKeyPair::from_private_key_and_public_key(
        algorithm,
        private_key.as_ref(),
        &public_key,
    )
    .unwrap();
    let random = aws_lc_rs::rand::SystemRandom::new();
    let sign_once = || key_pair.sign(&random, string_to_sign.as_bytes()).unwrap();
    let (first, second) = (hex::encode(sign_once()), hex::encode(sign_once()));
    assert_ne!(first, second, "each signature takes a fresh nonce");
    for signature in [first, second] {
        let resigned = with_authorization(parts.clone(), &signed.value, &signature);
        assert_eq!(verdict(&resigned, &body, "cloudfront-keyvaluestore"), Ok(()));
    }
}

/// The AWS signing test suite (awslabs/aws-c-auth, as aws-sigv4 1.6.5 ships it): requests
/// signed by the CRT, each with its own random nonce, for `AKIDEXAMPLE` on 2015-08-30.
const SUITE_KEY: &str = "AKIDEXAMPLE";
const SUITE_SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
const SUITE_PUBLIC_KEY_X: &str = "b6618f6a65740a99e650b33b6b4b5bd0d43b176d721a3edfea7e7d2d56d936b1";
const SUITE_PUBLIC_KEY_Y: &str = "865ed22a7eadc9c5cb9d2cbaca1b3699139fedc5043dc6661864218330c8e518";

struct SuiteRequest {
    name: &'static str,
    method: &'static str,
    path_and_query: &'static str,
    headers: &'static [(&'static str, &'static str)],
    body: &'static [u8],
    signed_headers: &'static str,
    signature: &'static str,
}

const SUITE: [SuiteRequest; 4] = [
    SuiteRequest {
        name: "get-vanilla",
        method: "GET",
        path_and_query: "/",
        headers: &[],
        body: b"",
        signed_headers: "host;x-amz-date;x-amz-region-set",
        signature: "3046022100fddaaf816a31e30d04973875b13bc27b98da10907a3a1872fd5819a221334abc\
                    022100de2ee81d83b54f31efc82834de07e6cd4d5c6f25bfa08e0f29a15bb0977c343e",
    },
    SuiteRequest {
        name: "get-vanilla-query-order-key-case",
        method: "GET",
        path_and_query: "/?Param2=value2&Param1=value1",
        headers: &[],
        body: b"",
        signed_headers: "host;x-amz-date;x-amz-region-set",
        signature: "30450220394fa0699225f403514cb4db03b7c479f64c5f9864bf520b871bb23a054d6517\
                    022100d4445c909fc62a43c6c485a8582621e0e14c6ef1e4218072bc2dcaffc1188f18",
    },
    SuiteRequest {
        name: "get-relative-relative-normalized",
        method: "GET",
        path_and_query: "/example1/example2/../..",
        headers: &[],
        body: b"",
        signed_headers: "host;x-amz-date;x-amz-region-set",
        signature: "304402203edb747d59fe32db47529b2e5e02c9276759b7c43ad9e36ff62d52b8dbeb00e3\
                    022007f99e8601938b893fe6dcbac9259fec2f3797b0e3d74fd66d59b09e83b8c1d4",
    },
    SuiteRequest {
        name: "post-x-www-form-urlencoded",
        method: "POST",
        path_and_query: "/",
        headers: &[
            ("content-type", "application/x-www-form-urlencoded"),
            ("content-length", "13"),
            (
                "x-amz-content-sha256",
                "9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e",
            ),
        ],
        body: b"Param1=value1",
        signed_headers: "content-length;content-type;host;x-amz-content-sha256;x-amz-date;\
                         x-amz-region-set",
        signature: "3045022100955bbc0f0b9d4284719808642167f7d5ea4a72f8c296b75b442898c8b81cd7e5\
                    02202425fb97620fa9a20344d22b248dd00db07524fc39b5ad77533e00b277331241",
    },
];

fn suite_parts(suite: &SuiteRequest) -> http::request::Parts {
    let authorization = format!(
        "AWS4-ECDSA-P256-SHA256 Credential={SUITE_KEY}/20150830/service/aws4_request, \
         SignedHeaders={}, Signature={}",
        suite.signed_headers, suite.signature
    );
    let mut builder = http::Request::builder()
        .method(suite.method)
        .uri(suite.path_and_query)
        .header("host", "example.amazonaws.com")
        .header("x-amz-date", "20150830T123600Z")
        .header("x-amz-region-set", "us-east-1")
        .header("authorization", authorization);
    for (name, value) in suite.headers {
        builder = builder.header(*name, *value);
    }
    builder.body(()).unwrap().into_parts().0
}

#[test]
fn the_public_key_is_derived_as_the_aws_signing_test_suite_derives_it() {
    let public_key = sigv4a::public_key(SUITE_KEY, SUITE_SECRET).unwrap();
    let expected = format!("04{SUITE_PUBLIC_KEY_X}{SUITE_PUBLIC_KEY_Y}");
    assert_eq!(hex::encode(public_key), expected);
}

#[test]
fn the_aws_signing_test_suite_sigv4a_signatures_verify_and_tampered_copies_do_not() {
    for suite in &SUITE {
        let parts = suite_parts(suite);
        let matches = |parts: &http::request::Parts, body: &[u8]| {
            let signed = Signature::of(parts).unwrap();
            sigv4a::signature_matches(parts, body, &signed, SUITE_KEY, SUITE_SECRET)
        };
        assert!(matches(&parts, suite.body), "{}", suite.name);
        let retargeted = with_uri(parts.clone(), "/", "/other/");
        assert!(!matches(&retargeted, suite.body), "{}: retargeted", suite.name);
        let redated = with_header(parts.clone(), "x-amz-date", "20150830T123601Z");
        assert!(!matches(&redated, suite.body), "{}: another date", suite.name);
        let other_secret = Signature::of(&parts).unwrap();
        let by_other = sigv4a::signature_matches(&parts, suite.body, &other_secret, SUITE_KEY, "x");
        assert!(!by_other, "{}: another secret", suite.name);
    }
}

/// The canonical request SigV4A signs is SigV4's: this checks the one the verifier builds
/// against aws-sigv4's SigV4 signer, over the request shapes the SigV4 cases above cover.
#[test]
fn the_canonical_request_is_the_one_the_sigv4_signer_signs() {
    let s3 = Client { service: "s3", ..SQS };
    let hashed = Client { hashed_payload: true, ..s3 };
    let query = || request("GET", "https://127.0.0.1:8443/a/./b/../c?b=2&a=1&a=0&c=x%2By%20z&d", b"");
    #[rustfmt::skip]
    let cases: Vec<(&str, (http::request::Parts, Vec<u8>), Vec<u8>)> = vec![
        ("sqs post", signed(&SQS, send_message()), br#"{"QueueUrl":"q"}"#.to_vec()),
        ("in-process, no host header", signed(&SQS, in_process()), vec![]),
        ("query with repeated keys, dot segments", signed(&SQS, query()), vec![]),
        ("kvs path with an encoded ARN", signed(&Client { service: "cloudfront-keyvaluestore", ..SQS }, put_key()), PUT_KEY_BODY.to_vec()),
        ("s3 key with a space", signed(&s3, get_object()), vec![]),
        ("s3 key holding a literal %2F", signed(&s3, s3_get("a%252Fb")), vec![]),
        ("s3 key with ! + space ~ and UTF-8", signed(&s3, s3_get("%21%2B%20~%D0%BA")), vec![]),
        ("s3 put with hashed payload", signed(&hashed, request("PUT", "https://127.0.0.1:8443/bucket/key", b"object")), b"object".to_vec()),
    ];
    for (name, (parts, _), body) in cases {
        let signed = Signature::of(&parts).unwrap();
        let canonical_request = sigv4a::canonical_request(&parts, &body, &signed).unwrap();
        let [_, date, region, service, _] = &signed.scope[..] else { panic!("{name}") };
        let hashed_request = hex::encode(aws_lc_rs::digest::digest(
            &aws_lc_rs::digest::SHA256,
            canonical_request.as_bytes(),
        ));
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{date}/{region}/{service}/aws4_request\n{hashed_request}",
            signed.amz_date
        );
        let at = parse_amz_date(&signed.amz_date).unwrap();
        let key = aws_sigv4::sign::v4::generate_signing_key(SECRET, at, region, service);
        let signature = aws_sigv4::sign::v4::calculate_signature(key, string_to_sign.as_bytes());
        assert_eq!(signature, signed.value, "{name}:\n{canonical_request}");
    }
}

#[test]
fn a_chunk_signed_sigv4a_stream_is_refused_since_its_chunks_are_not_verified() {
    let marker = "STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD";
    let s3_put = request("PUT", "https://127.0.0.1:8443/bucket/key", b"");
    let mut s3_put = s3_put;
    let value = http::HeaderValue::from_static(marker);
    s3_put.headers_mut().insert("x-amz-content-sha256", value);
    let (parts, _) = sigv4a_signed(&Client { service: "s3", ..KVS }, s3_put);
    let signed = Signature::of(&parts).unwrap();
    assert_eq!(sigv4a::canonical_request(&parts, b"", &signed), None);
    assert_eq!(verdict(&parts, b"", "s3"), Err("SignatureDoesNotMatch"));
}
