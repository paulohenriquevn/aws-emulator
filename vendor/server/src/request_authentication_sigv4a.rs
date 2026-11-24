// SPDX-License-Identifier: Apache-2.0
//! fork: SigV4A (`AWS4-ECDSA-P256-SHA256`) request verification.
//!
//! An SDK signs a SigV4A request with an ECDSA P-256 key it derives from the secret access key
//! (`aws_sigv4::sign::v4a::generate_signing_key`), over SHA-256 of a string to sign whose
//! credential scope has no region field:
//!
//! ```text
//! AWS4-ECDSA-P256-SHA256
//! <x-amz-date>
//! <date>/<signing name>/aws4_request
//! <hex SHA-256 of the canonical request>
//! ```
//!
//! The canonical request is SigV4's. The region set the signature covers travels in the
//! `X-Amz-Region-Set` header, which is signed like any other header.
//!
//! ECDSA signatures are randomized (the Go SDK and the CRT behind the AWS CLI draw a fresh
//! nonce for each one), so a request cannot be verified by signing it again, as SigV4 is. Here
//! the canonical request is rebuilt from the request as received, the public key is derived
//! from the credential's secret, and the hex DER signature is verified with aws-lc-rs.
//! aws-sigv4 keeps its canonical request builder private, so this one reuses the SigV4 path's
//! pieces (the S3 path encoding, the payload rules, the encode sets) and is held to aws-sigv4's
//! SigV4 signer by `the_canonical_request_is_the_one_the_sigv4_signer_signs`.
//!
//! Only the `Authorization` header form is verified. A presigned SigV4A URL is not recognized,
//! so it is refused like any request no authenticator recognizes.

use super::{Parts, Signature, payload, signable_path, signs_like_s3};
use super::{QUERY_ENCODE_SET, S3_PATH_ENCODE_SET, SIGV4A_ALGORITHM, UNSIGNED_PAYLOAD};
use aws_sigv4::http_request::SignableBody;
use std::fmt::Write as _;

const X_AMZ_REGION_SET: &str = "x-amz-region-set";

/// The payload marker of a chunk-signed SigV4A stream. Its chunk signatures are not verified
/// here, so such a request is refused rather than served with its chunks unchecked.
const STREAMING_ECDSA_PAYLOAD: &str = "STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD";

/// Whether the request names the region set its signature covers, and signs it.
pub(super) fn region_set_signed(parts: &Parts, signed: &Signature) -> bool {
    let named = parts.headers.get(X_AMZ_REGION_SET);
    let named = named.is_some_and(|value| !value.is_empty());
    named && signed.headers.iter().any(|name| name == X_AMZ_REGION_SET)
}

/// Whether `signed.value` is the signature of the request by the key pair of `key_id` and
/// `secret`.
pub(super) fn signature_matches(
    parts: &Parts,
    body: &[u8],
    signed: &Signature,
    key_id: &str,
    secret: &str,
) -> bool {
    let string_to_sign = string_to_sign(parts, body, signed);
    let public_key = public_key(key_id, secret);
    let signature = hex::decode(&signed.value);
    let (Some(string_to_sign), Some(public_key), Ok(signature)) =
        (string_to_sign, public_key, signature)
    else {
        return false;
    };
    let algorithm = &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1;
    let public_key = aws_lc_rs::signature::UnparsedPublicKey::new(algorithm, public_key);
    public_key
        .verify(string_to_sign.as_bytes(), &signature)
        .is_ok()
}

/// The string a SigV4A request is signed over. The scope's date is the one `x-amz-date` names,
/// so a scope dated otherwise does not verify.
pub(super) fn string_to_sign(parts: &Parts, body: &[u8], signed: &Signature) -> Option<String> {
    let [_, _, service, terminator] = &signed.scope[..] else {
        return None;
    };
    let amz_date = &signed.amz_date;
    let date = amz_date.get(..8)?;
    let hashed_request = sha256_hex(canonical_request(parts, body, signed)?.as_bytes());
    Some(format!(
        "{SIGV4A_ALGORITHM}\n{amz_date}\n{date}/{service}/{terminator}\n{hashed_request}"
    ))
}

/// The uncompressed SEC1 public key (`04 || X || Y`) of the P-256 key pair SigV4A derives from
/// `key_id` and `secret`.
pub(super) fn public_key(key_id: &str, secret: &str) -> Option<Vec<u8>> {
    let private_key = aws_sigv4::sign::v4a::generate_signing_key(key_id, secret);
    let algorithm = &aws_lc_rs::agreement::ECDH_P256;
    let private_key =
        aws_lc_rs::agreement::PrivateKey::from_private_key(algorithm, private_key.as_ref());
    let public_key = private_key.ok()?.compute_public_key().ok()?;
    Some(public_key.as_ref().to_vec())
}

/// SigV4's canonical request for the request as received: method, path, query, the headers
/// `signed` names with their values, the signed header list and the payload hash. `None` when a
/// signed header is missing or not text, or the payload is a chunk-signed SigV4A stream.
pub(super) fn canonical_request(
    parts: &Parts,
    body: &[u8],
    signed: &Signature,
) -> Option<String> {
    let mut canonical = format!(
        "{}\n{}\n{}\n",
        parts.method,
        canonical_path(parts, signed),
        canonical_query(parts.uri.query().unwrap_or(""))
    );
    for name in &signed.headers {
        let value = canonical_header_value(parts, name)?;
        writeln!(canonical, "{name}:{value}").ok()?;
    }
    let payload_hash = payload_hash(parts, body, signed)?;
    write!(canonical, "\n{}\n{payload_hash}", signed.headers.join(";")).ok()?;
    Some(canonical)
}

/// An S3-family path is signed as the SigV4 path signs it ([`signable_path`]); any other is
/// normalized and encoded again, so `%3A` signs as `%253A`.
fn canonical_path(parts: &Parts, signed: &Signature) -> String {
    if signs_like_s3(signed.signing_name()) {
        return signable_path(parts, signed).into_owned();
    }
    let normalized = normalize_path(parts.uri.path());
    percent_encoding::utf8_percent_encode(&normalized, S3_PATH_ENCODE_SET).to_string()
}

/// RFC 3986 dot-segment removal, with empty segments dropped and a trailing slash kept, as
/// aws-sigv4 normalizes a path before encoding it.
fn normalize_path(path: &str) -> String {
    if !(path.contains('.') || path.contains("//")) {
        return match path.starts_with('/') {
            true => path.to_string(),
            false => format!("/{path}"),
        };
    }
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    let mut normalized = format!("/{}", segments.join("/"));
    let trailing = ["/", "/.", "/..", "/./", "/../"];
    if trailing.iter().any(|end| path.ends_with(end)) && !normalized.ends_with('/') {
        normalized.push('/');
    }
    normalized
}

/// The query's name-value pairs, each decoded as a form field and encoded again with SigV4's
/// `UriEncode`, sorted, and joined as `name=value` with `&`.
fn canonical_query(query: &str) -> String {
    let decode = |text: &str| {
        let text = text.replace('+', " ");
        percent_encoding::percent_decode_str(&text)
            .decode_utf8_lossy()
            .into_owned()
    };
    let encode = |text: &str| {
        percent_encoding::utf8_percent_encode(&decode(text), QUERY_ENCODE_SET).to_string()
    };
    let items = query.split('&').filter(|item| !item.is_empty());
    let mut pairs: Vec<(String, String)> = items
        .map(|item| {
            let (name, value) = item.split_once('=').unwrap_or((item, ""));
            (encode(name), encode(value))
        })
        .collect();
    pairs.sort();
    let pairs: Vec<String> = pairs.iter().map(|(name, value)| format!("{name}={value}")).collect();
    pairs.join("&")
}

/// Every value the request carries for `name`, trimmed and with runs of spaces collapsed, joined
/// with `,`. A request without a `Host` header (in-process, HTTP/2) signs its URI authority,
/// without the scheme's default port.
fn canonical_header_value(parts: &Parts, name: &str) -> Option<String> {
    let values = parts.headers.get_all(name).iter();
    let values: Option<Vec<String>> = values
        .map(|value| Some(collapse_spaces(value.to_str().ok()?)))
        .collect();
    let values = values?;
    if !values.is_empty() {
        return Some(values.join(","));
    }
    if name != "host" {
        return None;
    }
    let authority = parts.uri.authority()?;
    let default_port = matches!(
        (parts.uri.scheme_str(), authority.port_u16()),
        (Some("http"), Some(80)) | (Some("https"), Some(443))
    );
    Some(match default_port {
        true => authority.host().to_string(),
        false => authority.as_str().to_string(),
    })
}

fn collapse_spaces(value: &str) -> String {
    let mut collapsed = String::with_capacity(value.len());
    for character in value.trim_matches(' ').chars() {
        if !(character == ' ' && collapsed.ends_with(' ')) {
            collapsed.push(character);
        }
    }
    collapsed
}

/// The payload hash the SigV4 path signs ([`payload`]): a declared marker as written, else the
/// body's own hash.
fn payload_hash(parts: &Parts, body: &[u8], signed: &Signature) -> Option<String> {
    match payload(parts, body, signed) {
        SignableBody::Bytes(body) => Some(sha256_hex(body)),
        SignableBody::Precomputed(marker) if marker.starts_with(STREAMING_ECDSA_PAYLOAD) => None,
        SignableBody::Precomputed(marker) => Some(marker),
        SignableBody::UnsignedPayload => Some(UNSIGNED_PAYLOAD.to_string()),
        _ => None,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes))
}
