// SPDX-License-Identifier: CC0-1.0

//! The emulator's TLS certificate verifies every host a theo client dials.
//!
//! S3 Control prefixes the endpoint host with the account id (aws/aws-emulator#56), so a
//! client pointed at `https://localhost:<port>` dials `000000000000.localhost:<port>` for
//! it. The certificate must verify that name as well as `localhost` itself.

use std::sync::Arc;

use emulator_server::TlsCertificateAuthority;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

fn verifies(host: &str) -> bool {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let der = CertificateDer::from(TlsCertificateAuthority::for_this_process().der().to_vec());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der.clone()).expect("the emulator certificate is a valid trust anchor");
    let verifier = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
        .build()
        .expect("a verifier over one root");
    let name = ServerName::try_from(host.to_string()).expect("a valid server name");
    verifier
        .verify_server_cert(&der, &[], &name, &[], UnixTime::now())
        .is_ok()
}

#[test]
fn the_certificate_verifies_localhost() {
    assert!(verifies("localhost"));
}

#[test]
fn the_certificate_verifies_an_account_prefixed_localhost_host() {
    assert!(verifies("000000000000.localhost"));
}

#[test]
fn the_certificate_still_refuses_a_host_it_does_not_name() {
    assert!(!verifies("emulator.example.org"));
}

/// A resource whose configured port is taken on 127.0.0.1 listens on an extra loopback
/// address (`127.0.0.2` to `127.0.0.16`), and a client verifying TLS there must succeed.
#[test]
fn the_certificate_verifies_the_extra_loopback_addresses() {
    for host in ["127.0.0.2", "127.0.0.16"] {
        assert!(verifies(host), "{host}");
    }
    assert!(!verifies("127.0.0.17"), "127.0.0.17 is not one of them");
}
