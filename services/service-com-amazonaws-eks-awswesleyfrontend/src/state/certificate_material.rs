// SPDX-License-Identifier: CC0-1.0

//! The X.509 material a cluster's certificate authorities are made of.
//!
//! `Certificate$data` is "the Base64-encoded certificate data required to
//! communicate with your cluster. Add this to the `certificate-authority-data`
//! section of the kubeconfig file for your cluster"
//! (`smithy_query(query="shape", shape="Certificate")`), and a
//! `CertificateAuthority$data` is the same thing for one certificate authority of
//! the cluster — so what the member carries has to be a certificate a client can
//! decode, parse and install as a trust anchor, not text in the shape of one.
//! Every client built from a cluster's own kubeconfig reads it that way.
//!
//! A child module of [`super`] rather than more of `state.rs`: issuing material
//! is one concern, and it is the only part of this service that holds a private
//! key at all. The key is generated here and kept on the certificate authority's
//! record as a [`SigningKeyPem`], whose `Debug` never prints it, because a control
//! plane provisioned OUTSIDE the emulator (`EMULATOR_EKS_CONTROL_PLANE=external`)
//! has to sign with the very CA `DescribeCluster` reports, as an Amazon EKS
//! control plane signs with the CA Amazon EKS issued it. No shape of this service
//! has a member for a private key, so no API operation reports it; only the fork's
//! control-plane registration route hands it to the provisioner.

use super::*;

/// The subject common name a cluster's certificate authority is issued under.
///
/// Kubernetes names a cluster's own root of trust `kubernetes`, which is the
/// subject every client that inspects the CA of a cluster sees — so a certificate
/// authority of this service is issued under the same name rather than under a
/// name this emulator invents.
pub const CERTIFICATE_AUTHORITY_COMMON_NAME: &str = "kubernetes";

/// fork: the private key a certificate authority signs with, PEM encoded (PKCS #8).
///
/// Its `Debug` prints no key material, so a record logged whole leaks nothing.
#[derive(Clone, PartialEq, Eq)]
pub struct SigningKeyPem(String);

impl SigningKeyPem {
    /// The PEM text, for the one reader allowed to hand it out: the provisioner of an
    /// external control plane.
    pub fn pem(&self) -> &str {
        &self.0
    }
}

impl ::std::fmt::Debug for SigningKeyPem {
    fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        formatter.write_str("SigningKeyPem(<redacted>)")
    }
}

/// What a certificate authority is made of: its `data`, the Base64 of the PEM-encoded
/// X.509 certificate, and the key that certificate's public half belongs to.
#[derive(Debug, Clone)]
pub struct CertificateAuthorityMaterial {
    pub data: String,
    pub signing_key: SigningKeyPem,
}

/// A certificate authority issued over the validity period the record reports.
///
/// A REAL self-signed certificate authority certificate, issued through the
/// maintained generator this tree already uses for certificate material: it is
/// marked as a CA and carries the key usages a CA signs certificates and
/// revocation lists with, so a client that Base64-decodes `data` gets a PEM
/// block it can parse and install as a trust anchor, which is what a kubeconfig
/// built from `DescribeCluster` does with it.
///
/// `not_before`/`not_after` are the record's own validity period
/// ([`CertificateAuthorityRecord::not_before`]), so the window the certificate
/// bears and the one `DescribeCertificateAuthority` reports are one value. The
/// material is issued ONCE, when the certificate authority is created, and stored
/// on the record; no read re-issues it, so every read reports the same
/// certificate.
///
/// `None` when the material does not issue, which the operation that asked for it
/// reports rather than storing a certificate authority with nothing in it.
pub fn mint_certificate_authority(
    not_before: SystemTime,
    not_after: SystemTime,
) -> Option<CertificateAuthorityMaterial> {
    match issue_certificate_authority_pem(not_before, not_after) {
        Some((certificate, key)) => Some(CertificateAuthorityMaterial {
            data: ::aws_smithy_types::base64::encode(certificate),
            signing_key: SigningKeyPem(key),
        }),
        None => {
            // The caller answers its modelled server error for this, which says
            // nothing about WHY: the generator's own failure is worth a line so
            // the path is diagnosable if it ever fires.
            ::tracing::warn!("the certificate authority's certificate could not be issued");
            None
        }
    }
}

/// The PEM-encoded certificate and the PEM-encoded key it was self-signed with.
fn issue_certificate_authority_pem(
    not_before: SystemTime,
    not_after: SystemTime,
) -> Option<(String, String)> {
    let key = ::rcgen::KeyPair::generate().ok()?;
    // No subject alternative name: a certificate authority certifies the names its
    // issued certificates carry, it does not serve one itself.
    let mut params = ::rcgen::CertificateParams::new(Vec::<String>::new()).ok()?;
    let mut subject = ::rcgen::DistinguishedName::new();
    subject.push(
        ::rcgen::DnType::CommonName,
        CERTIFICATE_AUTHORITY_COMMON_NAME,
    );
    params.distinguished_name = subject;
    params.not_before = offset_date_time(not_before)?;
    params.not_after = offset_date_time(not_after)?;
    params.is_ca = ::rcgen::IsCa::Ca(::rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        ::rcgen::KeyUsagePurpose::KeyCertSign,
        ::rcgen::KeyUsagePurpose::CrlSign,
        ::rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    let certificate = params.self_signed(&key).ok()?.pem();
    Some((certificate, key.serialize_pem()))
}

/// `at` as the date-time type the certificate generator takes a validity bound
/// in, at the one-second resolution an X.509 validity period is stated at.
///
/// `None` for an instant outside the range that type represents, which is the
/// same answer the caller gives for material that does not issue.
fn offset_date_time(at: SystemTime) -> Option<::time::OffsetDateTime> {
    let seconds = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|since_epoch| i64::try_from(since_epoch.as_secs()).ok())?;
    ::time::OffsetDateTime::from_unix_timestamp(seconds).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::x509_cert::der::{DecodePem, Encode};

    fn minted() -> CertificateAuthorityMaterial {
        let now = SystemTime::now();
        mint_certificate_authority(now, now + Duration::from_secs(3600)).expect("material that issues")
    }

    #[test]
    fn the_signing_key_is_the_private_half_of_the_certificate() {
        let material = minted();

        let pem = String::from_utf8(::aws_smithy_types::base64::decode(&material.data).expect("Base64")).expect("UTF-8");
        let certificate = ::x509_cert::Certificate::from_pem(pem.as_bytes()).expect("a PEM certificate");
        let certified = certificate.tbs_certificate.subject_public_key_info.to_der().expect("DER");
        let key = ::rcgen::KeyPair::from_pem(material.signing_key.pem()).expect("a PEM private key");

        assert_eq!(key.public_key_der(), certified);
    }

    #[test]
    fn a_debug_print_of_the_material_holds_no_key() {
        let material = minted();

        let printed = format!("{material:?}");

        assert!(!printed.contains("PRIVATE KEY"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
    }
}
