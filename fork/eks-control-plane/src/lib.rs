// SPDX-License-Identifier: CC0-1.0

//! `/_fork/eks/clusters/{name}/control-plane`: the seam between an emulated EKS cluster and
//! the Kubernetes control plane a provisioner outside the emulator runs for it (a kind or
//! k3d cluster), as Amazon EKS provisions a real cluster's control plane behind the API.
//!
//! * `GET` answers what the provisioner needs to build that control plane: the cluster's
//!   status, Kubernetes version and OIDC issuer (the API server's
//!   `--service-account-issuer`), and the certificate authority Amazon EKS issued the
//!   cluster, certificate and signing key, so the API server's certificates chain to the
//!   `certificateAuthority.data` that `DescribeCluster` reports. It answers while the
//!   cluster is still `CREATING`, which is when the provisioner needs it.
//! * `PUT {"endpoint": "https://host:port"}` records the API server the provisioner
//!   brought up. The cluster's `endpoint` becomes that address and the cluster turns
//!   `ACTIVE`, which is what provisioning the control plane completes on Amazon EKS. With
//!   `EMULATOR_EKS_CONTROL_PLANE=external` this call is the ONLY thing that activates a
//!   cluster; without it, it updates the endpoint of a cluster the clock activated.
//!
//! Both are signed with SigV4 (signing name `eks`) by the emulator's own keys, the ones in
//! `emulator_credentials`, and the Region of the signature is the cluster's Region. They
//! are not AWS APIs: AWS keeps a real cluster's control plane, and its CA key, to itself.
//!
//! What this does not model: a successor CA a caller appends and activates
//! (`CreateCertificateAuthority`, `ActivateCertificateAuthority`) is not rolled into the
//! external control plane. `GET` hands out the CA that is signing, so a provisioner that
//! rebuilds after an activation picks the new one up; one that does not keeps signing
//! with the old one.

use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use emulator_server::{CredentialStore, RequestAuthenticator, SigV4Authenticator};
use engine::InMemoryEngine;
use http::{header, Method, Request, Response, StatusCode, Uri};
use service_com_amazonaws_eks_awswesleyfrontend::state::{
    self as eks, ClusterRecord, CA_SIGNING_IN_USE, STATUS_ACTIVE, STATUS_DELETING,
};

/// The path prefix every cluster route of the fork shares.
pub const PATH_PREFIX: &str = "/_fork/eks/clusters/";

/// The last path segment of this route.
const SEGMENT: &str = "control-plane";

/// The signing name the request must be signed for.
const SIGNING_NAME: &str = "eks";

/// The Region a request names when its signature carries none (authentication off).
const DEFAULT_REGION: &str = "us-east-1";

/// The largest body accepted: an endpoint is one URL.
const MAX_BODY_BYTES: usize = 4 * 1024;

/// Whether the router should hand `request` to [`respond`].
pub fn claims(request: &Request<Bytes>) -> bool {
    cluster_named(request.uri().path()).is_some()
}

/// The cluster name in `/_fork/eks/clusters/{name}/control-plane`.
pub fn cluster_named(path: &str) -> Option<&str> {
    let (name, segment) = path.strip_prefix(PATH_PREFIX)?.split_once('/')?;
    (segment == SEGMENT && !name.is_empty()).then_some(name)
}

/// Answer a `GET` or a `PUT` of the control plane of the cluster `request`'s path names.
pub fn respond(engine: &Arc<InMemoryEngine>, request: &Request<Bytes>) -> Response<Bytes> {
    let Some(cluster_name) = cluster_named(request.uri().path()) else {
        return answer(StatusCode::NOT_FOUND, &format!("no route for {}; expected {PATH_PREFIX}<cluster>/{SEGMENT}", request.uri().path()));
    };
    if let Err(refusal) = authenticated(request) {
        return refusal;
    }
    let region = emulator_server::sigv4_credential_scope(request.headers())
        .map_or_else(|| DEFAULT_REGION.to_string(), |scope| scope.region);
    match *request.method() {
        Method::GET => describe(engine, &region, cluster_name),
        Method::PUT => register(engine, &region, cluster_name, request.body()),
        _ => answer(StatusCode::METHOD_NOT_ALLOWED, "only GET and PUT are served"),
    }
}

fn describe(engine: &Arc<InMemoryEngine>, region: &str, name: &str) -> Response<Bytes> {
    with_cluster(engine, region, name, |cluster| {
        let Some(issuer) = cluster.identity.as_ref().and_then(|identity| identity.oidc.as_ref()).and_then(|oidc| oidc.issuer.clone()) else {
            return answer(StatusCode::BAD_REQUEST, &format!("cluster {name} has no OIDC issuer: it is a connected cluster, which Amazon EKS provisions no control plane for"));
        };
        let Some(authority) = cluster.certificate_authorities.values().find(|authority| authority.signing_status == CA_SIGNING_IN_USE) else {
            return answer(StatusCode::CONFLICT, &format!("cluster {name} has no certificate authority signing"));
        };
        json(
            StatusCode::OK,
            &serde_json::json!({
                "name": cluster.name,
                "status": cluster.status,
                "version": cluster.version,
                "issuer": issuer,
                "endpoint": cluster.control_plane_provisioned.then(|| cluster.endpoint.clone()).flatten(),
                "certificateAuthority": {
                    "id": authority.id,
                    "data": authority.data,
                    "signingKey": authority.signing_key.pem(),
                },
            }),
        )
    })
}

fn register(engine: &Arc<InMemoryEngine>, region: &str, name: &str, body: &Bytes) -> Response<Bytes> {
    if body.len() > MAX_BODY_BYTES {
        return answer(StatusCode::PAYLOAD_TOO_LARGE, &format!("the body is over {MAX_BODY_BYTES} bytes"));
    }
    let endpoint = match registered_endpoint(body) {
        Ok(endpoint) => endpoint,
        Err(defect) => return answer(StatusCode::BAD_REQUEST, &defect),
    };
    with_cluster(engine, region, name, |cluster| {
        if cluster.connector_config.is_some() {
            return answer(StatusCode::BAD_REQUEST, &format!("cluster {name} is a connected cluster, which Amazon EKS provisions no control plane for"));
        }
        if cluster.status == STATUS_DELETING {
            return answer(StatusCode::CONFLICT, &format!("cluster {name} is DELETING"));
        }
        cluster.endpoint = Some(endpoint.clone());
        cluster.status = STATUS_ACTIVE.to_string();
        cluster.active_at = None;
        cluster.control_plane_provisioned = true;
        json(StatusCode::OK, &serde_json::json!({ "name": cluster.name, "status": cluster.status, "endpoint": endpoint }))
    })
}

/// The cluster `name` in `region`, with the transitions the clock owes it applied, handed
/// to `act`; 404 when there is none.
fn with_cluster(engine: &Arc<InMemoryEngine>, region: &str, name: &str, act: impl FnOnce(&mut ClusterRecord) -> Response<Bytes>) -> Response<Bytes> {
    let Some(state) = eks::service_state(&**engine) else {
        return answer(StatusCode::NOT_FOUND, &format!("no EKS cluster {name} in {region}"));
    };
    let mut store = state.lock();
    let Some(cluster) = store.clusters.get_mut(&(region.to_string(), name.to_string())) else {
        return answer(StatusCode::NOT_FOUND, &format!("no EKS cluster {name} in {region}"));
    };
    cluster.materialize_due_transitions(SystemTime::now());
    act(cluster)
}

/// The `endpoint` of a registration body: an `https` URL naming a host, with no path,
/// query or fragment, which is the form `DescribeCluster` reports an endpoint in.
pub fn registered_endpoint(body: &[u8]) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|error| format!("the body is not JSON: {error}"))?;
    let Some(text) = value.get("endpoint").and_then(serde_json::Value::as_str) else {
        return Err("the body has no string member `endpoint`".to_string());
    };
    let uri: Uri = text.parse().map_err(|error| format!("endpoint {text:?} is not a URL: {error}"))?;
    if uri.scheme_str() != Some("https") {
        return Err(format!("endpoint {text:?} is not https: an EKS API server is only served over TLS"));
    }
    let Some(authority) = uri.authority().filter(|authority| !authority.host().is_empty()) else {
        return Err(format!("endpoint {text:?} names no host"));
    };
    if !matches!(uri.path(), "" | "/") || uri.query().is_some() {
        return Err(format!("endpoint {text:?} has a path or a query; an API server endpoint is https://host[:port]"));
    }
    Ok(format!("https://{authority}"))
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
    fn the_cluster_name_is_read_from_the_control_plane_path() {
        assert_eq!(cluster_named("/_fork/eks/clusters/core/control-plane"), Some("core"));
    }

    #[test]
    fn a_path_naming_another_resource_is_not_claimed() {
        for path in [
            "/_fork/eks/clusters//control-plane",
            "/_fork/eks/clusters/core",
            "/_fork/eks/clusters/core/service-account-issuer-keys",
            "/_fork/eks/clusters/core/control-plane/extra",
        ] {
            assert_eq!(cluster_named(path), None, "{path}");
        }
    }

    #[test]
    fn an_https_endpoint_with_a_port_is_registered_as_given() {
        assert_eq!(registered_endpoint(br#"{"endpoint": "https://172.29.10.10:6443"}"#), Ok("https://172.29.10.10:6443".to_string()));
    }

    #[test]
    fn a_trailing_slash_is_dropped_from_the_endpoint() {
        assert_eq!(registered_endpoint(br#"{"endpoint": "https://api.core.internal/"}"#), Ok("https://api.core.internal".to_string()));
    }

    #[test]
    fn an_endpoint_that_is_not_https_is_refused() {
        let refused = registered_endpoint(br#"{"endpoint": "http://172.29.10.10:6443"}"#).expect_err("plain http");

        assert!(refused.contains("not https"), "{refused}");
    }

    #[test]
    fn an_endpoint_with_a_path_is_refused() {
        assert!(registered_endpoint(br#"{"endpoint": "https://172.29.10.10:6443/api"}"#).is_err());
    }

    #[test]
    fn a_body_without_an_endpoint_is_refused() {
        let refused = registered_endpoint(br#"{"certificateAuthorityData": "x"}"#).expect_err("no endpoint");

        assert!(refused.contains("`endpoint`"), "{refused}");
    }
}
