// SPDX-License-Identifier: CC0-1.0

//! Pods of an EKS cluster reach the emulator as `aws.emulator.internal`: the eks-provisioner
//! maps that name and `000000000000.aws.emulator.internal` to the gateway in each pod's
//! `/etc/hosts` and points `AWS_ENDPOINT_URL` at the first. An SDK there calls the
//! CloudFront KeyValueStore data plane at the second, the endpoint host prefixed with the
//! account id, so the served data port must complete a TLS handshake under that name and
//! answer the request.
//!
//! The data port is served on a loopback port, and the client resolves the name to it, as
//! the pod's `/etc/hosts` does.

mod support;

use std::net::{Ipv4Addr, SocketAddr};

use support::{create_key_value_store, emulator_keys, on_authenticating_router, segment, signed_for, KEY_VALUE_STORE_DATA_PLANE};

/// The account-prefixed name an SDK in a pod dials for the key value store data plane.
const PREFIXED_NAME: &str = "000000000000.aws.emulator.internal";

/// A loopback port nothing listens on.
fn free_port() -> u16 {
    let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("a loopback port");
    probe.local_addr().expect("a bound address").port()
}

#[test]
fn the_key_value_store_data_plane_answers_at_the_account_prefixed_emulator_name() {
    // The served data port writes its client files under the home directory; this test's
    // go to the target directory rather than the developer's.
    std::env::set_var("HOME", env!("CARGO_TARGET_TMPDIR"));
    on_authenticating_router(|router, _engine| {
        Box::pin(async move {
            let store_arn = create_key_value_store(&router, "pod-routes").await;
            let port = free_port();
            tokio::spawn(emulator_server::serve_emulator_on(router, Ipv4Addr::LOCALHOST.into(), port));
            let root = emulator_server::TlsCertificateAuthority::for_this_process().der().to_vec();
            let client = reqwest::Client::builder()
                .tls_built_in_root_certs(false)
                .add_root_certificate(reqwest::Certificate::from_der(&root).expect("the emulator's certificate"))
                .resolve(PREFIXED_NAME, SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
                .build()
                .expect("a TLS client");
            let health = format!("https://{PREFIXED_NAME}:{port}/__admin/health");
            let mut last = String::from("no attempt");
            for _ in 0..100 {
                match client.get(&health).send().await {
                    Ok(answer) if answer.status().is_success() => {
                        last.clear();
                        break;
                    }
                    Ok(answer) => last = format!("status {}", answer.status()),
                    Err(error) => last = format!("{error:?}"),
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert!(last.is_empty(), "the data port did not answer under {PREFIXED_NAME}: {last}");

            let host = format!("{PREFIXED_NAME}:{port}");
            let (key, secret) = emulator_keys();
            let path = format!("/key-value-stores/{}", segment(&store_arn));
            let request = signed_for(&host, &KEY_VALUE_STORE_DATA_PLANE, (&key, &secret), "GET", &path, &[], "");
            let (parts, body) = request.into_parts();
            let mut headers = parts.headers;
            headers.remove("host");
            let answer = client
                .get(parts.uri.to_string())
                .headers(headers)
                .body(body)
                .send()
                .await
                .unwrap_or_else(|error| panic!("DescribeKeyValueStore at {host}: {error:?}"));

            let status = answer.status().as_u16();
            let text = answer.text().await.expect("a body");
            assert_eq!(status, 200, "DescribeKeyValueStore at {host}: {text}");
            assert!(text.contains(&store_arn), "{text}");
        })
    });
}
