// SPDX-License-Identifier: CC0-1.0

//! `PUT /_fork/eks/clusters/{name}/service-account-issuer-keys`: register the public key
//! set a local Kubernetes cluster (kind, k3d) signs its service account tokens with, as the
//! key set the emulated EKS cluster `{name}` publishes at its OIDC issuer URL.
//!
//! The local cluster plays the EKS cluster's control plane. It runs its API server with
//! `--service-account-issuer` set to the issuer URL `DescribeCluster` reports, and its key
//! set (`kubectl get --raw /openid/v1/jwks`) is registered here. From then on STS
//! (`AssumeRoleWithWebIdentity`, IRSA) and EKS Auth (`AssumeRoleForPodIdentity`) verify that
//! cluster's tokens the way AWS verifies a real cluster's.
//!
//! The request is signed with SigV4 (signing name `eks`) by the emulator's own keys, the
//! ones in `emulator_credentials`; the Region of the signature is the cluster's Region. It
//! is not an AWS API: AWS holds the key of a real cluster's control plane and needs no such
//! call. The answer is `{"issuer": "<url>"}` for a registered key set.

use std::sync::Arc;

use bytes::Bytes;
use emulator_server::{CredentialStore, RequestAuthenticator, SigV4Authenticator};
use engine::InMemoryEngine;
use http::{header, Method, Request, Response, StatusCode};
use service_common::service_account_issuers::ServiceAccountIssuerKeys;

/// The path prefix this crate serves.
pub const PATH_PREFIX: &str = "/_fork/eks/clusters/";

/// The last path segment of the registration.
const KEYS_SEGMENT: &str = "service-account-issuer-keys";

/// The signing name the request must be signed for.
const SIGNING_NAME: &str = "eks";

/// The Region a request names when its signature carries none (authentication off).
const DEFAULT_REGION: &str = "us-east-1";

/// The largest body accepted: a key set holds at most a few keys.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Whether the router should hand `request` to [`respond`].
pub fn claims(request: &Request<Bytes>) -> bool {
    request.uri().path().starts_with(PATH_PREFIX)
}

/// Register the key set in `request`'s body for the cluster its path names.
pub fn respond(engine: &Arc<InMemoryEngine>, request: &Request<Bytes>) -> Response<Bytes> {
    let Some(cluster_name) = cluster_named(request.uri().path()) else {
        return answer(StatusCode::NOT_FOUND, &format!("no route for {}; expected PUT {PATH_PREFIX}<cluster>/{KEYS_SEGMENT}", request.uri().path()));
    };
    if request.method() != Method::PUT {
        return answer(StatusCode::METHOD_NOT_ALLOWED, "only PUT is served");
    }
    if let Err(refusal) = authenticated(request) {
        return refusal;
    }
    if request.body().len() > MAX_BODY_BYTES {
        return answer(StatusCode::PAYLOAD_TOO_LARGE, &format!("the key set is over {MAX_BODY_BYTES} bytes"));
    }
    let key_set: serde_json::Value = match serde_json::from_slice(request.body()) {
        Ok(value) => value,
        Err(error) => return answer(StatusCode::BAD_REQUEST, &format!("the body is not JSON: {error}")),
    };
    let region = emulator_server::sigv4_credential_scope(request.headers())
        .map_or_else(|| DEFAULT_REGION.to_string(), |scope| scope.region);
    let Some(issuer) = cluster_issuer(engine, &region, cluster_name) else {
        return answer(StatusCode::NOT_FOUND, &format!("no EKS cluster {cluster_name} with an OIDC issuer in {region}"));
    };
    match engine.state::<ServiceAccountIssuerKeys>().lock().register(&issuer, key_set) {
        Ok(()) => json(StatusCode::OK, &serde_json::json!({ "issuer": issuer })),
        Err(defect) => answer(StatusCode::BAD_REQUEST, &defect.to_string()),
    }
}

/// The cluster name in `/_fork/eks/clusters/{name}/service-account-issuer-keys`.
pub fn cluster_named(path: &str) -> Option<&str> {
    let (name, segment) = path.strip_prefix(PATH_PREFIX)?.split_once('/')?;
    (segment == KEYS_SEGMENT && !name.is_empty()).then_some(name)
}

/// The OIDC issuer URL of the EKS cluster `name` in `region`.
fn cluster_issuer(engine: &Arc<InMemoryEngine>, region: &str, name: &str) -> Option<String> {
    let eks = service_com_amazonaws_eks_awswesleyfrontend::state::service_state(&**engine)?;
    let store = eks.lock();
    let cluster = store.clusters.get(&(region.to_string(), name.to_string()))?;
    cluster.identity.as_ref()?.oidc.as_ref()?.issuer.clone()
}

/// Refuse `request` unless authentication is off or it is signed by the emulator's keys.
fn authenticated(request: &Request<Bytes>) -> Result<(), Response<Bytes>> {
    if !emulator_server::authentication_enabled() {
        return Ok(());
    }
    let (parts, body) = request.clone().into_parts();
    let signer = SigV4Authenticator::for_this_process();
    if !signer.recognizes(&parts) {
        return Err(answer(StatusCode::FORBIDDEN, "sign the request with SigV4 (signing name eks) and the emulator's keys"));
    }
    signer
        .verify(&parts, &body, SIGNING_NAME, &CredentialStore::default())
        .map_err(|denied| answer(StatusCode::FORBIDDEN, &format!("{}: {}", denied.code, denied.message)))
}

fn answer(status: StatusCode, message: &str) -> Response<Bytes> {
    json(status, &serde_json::json!({ "message": message }))
}

fn json(status: StatusCode, body: &serde_json::Value) -> Response<Bytes> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Bytes::from(body.to_string()))
        .unwrap_or_else(|_| Response::new(Bytes::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cluster_name_is_read_from_the_registration_path() {
        assert_eq!(cluster_named("/_fork/eks/clusters/ops/service-account-issuer-keys"), Some("ops"));
    }

    #[test]
    fn a_path_naming_no_cluster_or_another_resource_names_no_cluster() {
        for path in [
            "/_fork/eks/clusters//service-account-issuer-keys",
            "/_fork/eks/clusters/ops",
            "/_fork/eks/clusters/ops/keys",
            "/_fork/eks/clusters/ops/service-account-issuer-keys/extra",
        ] {
            assert_eq!(cluster_named(path), None, "{path}");
        }
    }

    #[test]
    fn a_request_under_the_prefix_is_claimed_and_others_are_not() {
        let claimed = Request::builder().uri("/_fork/eks/clusters/ops/service-account-issuer-keys").body(Bytes::new()).expect("a request");
        let other = Request::builder().uri("/2015-12-08/clusters").body(Bytes::new()).expect("a request");

        assert!(claims(&claimed));
        assert!(!claims(&other));
    }
}
