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

mod support;

use serde_json::json;
use support::{
    create_key_value_store, emulator_keys, on_authenticating_router, segment, send, signed, Reply, Router, Scheme, HOST,
    KEY_VALUE_STORE_DATA_PLANE as DATA_PLANE_SIGV4A,
};

const ROUTE_KEY: &str = "tenant-a.example.com";

async fn data_plane(router: &Router, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let (key, secret) = emulator_keys();
    let mut all_headers = vec![("content-type", "application/json")];
    all_headers.extend_from_slice(headers);
    send(router, signed(&DATA_PLANE_SIGV4A, (&key, &secret), method, path, &all_headers, body)).await
}

#[test]
fn a_store_created_on_cloudfront_is_read_and_written_over_sigv4a() {
    on_authenticating_router(|router, _engine| {
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
    on_authenticating_router(|router, _engine| {
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
