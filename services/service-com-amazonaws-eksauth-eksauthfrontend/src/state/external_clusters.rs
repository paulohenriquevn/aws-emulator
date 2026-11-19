// SPDX-License-Identifier: CC0-1.0

//! fork: `AssumeRoleForPodIdentity` for an EKS cluster whose Kubernetes control plane runs
//! OUTSIDE the emulator.
//!
//! The cluster itself is an EKS cluster of the emulator (`CreateCluster`), and so are its
//! pod identity associations (`CreatePodIdentityAssociation`). Its service account tokens,
//! though, are signed by a local Kubernetes cluster (kind, k3d) that names the EKS
//! cluster's OIDC issuer URL as `iss` and whose public key set was registered for that
//! issuer ([`::service_common::service_account_issuers`]). A token is accepted here exactly
//! when AWS would accept a real cluster's: its signature checks out against the key set the
//! cluster's issuer publishes, it was issued by that issuer for the pod identity audience,
//! it is within its lifetime, and the service account it names has an association.
//!
//! A cluster registered in this crate's own records
//! ([`super::ComAmazonawsEksauthEksauthfrontendState::register_cluster`]) keeps the upstream
//! path; this one is taken only for a cluster the EKS service holds with a registered key
//! set.

use std::time::{SystemTime, UNIX_EPOCH};

use ::service_common::jwt;
use ::service_common::service_account_issuers::ServiceAccountIssuerKeys;

use super::{PodIdentityAssociationRecord, ServiceAccountIdentity, PODS_AUDIENCE};

/// The `sub` prefix a Kubernetes service account token carries:
/// `system:serviceaccount:<namespace>:<name>`.
const SERVICE_ACCOUNT_SUBJECT_PREFIX: &str = "system:serviceaccount:";

/// Why a token of an external cluster was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalRefusal {
    /// The signature, issuer, audience or subject does not hold.
    Invalid,
    /// The token's lifetime is over, or has not begun.
    Expired,
    /// The token is good, but no association binds the service account it names.
    NoAssociation(ServiceAccountIdentity),
}

/// The association and identity a token of an external cluster resolves to, `None` when
/// `cluster_name` in `region` is not an EKS cluster with a registered key set (the caller
/// then takes the upstream path).
pub fn authorize<Eng: Send + Sync + 'static>(
    engine: &Eng,
    region: &str,
    cluster_name: &str,
    token: &str,
    now: SystemTime,
) -> Option<Result<(PodIdentityAssociationRecord, ServiceAccountIdentity), ExternalRefusal>> {
    let in_memory = (engine as &dyn ::std::any::Any).downcast_ref::<::engine::InMemoryEngine>()?;
    let eks = ::service_com_amazonaws_eks_awswesleyfrontend::state::service_state(engine)?;
    let issuer = eks
        .lock()
        .clusters
        .get(&(region.to_string(), cluster_name.to_string()))?
        .identity
        .as_ref()?
        .oidc
        .as_ref()?
        .issuer
        .clone()?;
    let key_set = in_memory
        .state::<ServiceAccountIssuerKeys>()
        .lock()
        .key_set(&issuer)
        .cloned()?;
    Some(verified_identity(token, &key_set, &issuer, now).and_then(|identity| {
        association(engine, region, cluster_name, &identity)
            .map(|association| (association, identity.clone()))
            .ok_or(ExternalRefusal::NoAssociation(identity))
    }))
}

/// Who `token` names, when it is a pod identity token `issuer` signed with a key of
/// `key_set` and its lifetime covers `now`.
pub fn verified_identity(
    token: &str,
    key_set: &serde_json::Value,
    issuer: &str,
    now: SystemTime,
) -> Result<ServiceAccountIdentity, ExternalRefusal> {
    let verified = jwt::verify(token, key_set).map_err(|_| ExternalRefusal::Invalid)?;
    let issued_by = verified.claim_str("iss").unwrap_or_default().trim_end_matches('/');
    if issued_by != issuer.trim_end_matches('/') {
        return Err(ExternalRefusal::Invalid);
    }
    if !verified.audiences().contains(&PODS_AUDIENCE) {
        return Err(ExternalRefusal::Invalid);
    }
    let at = now.duration_since(UNIX_EPOCH).map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX));
    let expiry = verified.claims.get("exp").and_then(serde_json::Value::as_i64).ok_or(ExternalRefusal::Invalid)?;
    let not_before = verified.claims.get("nbf").and_then(serde_json::Value::as_i64);
    if at >= expiry || not_before.is_some_and(|not_before| at < not_before) {
        return Err(ExternalRefusal::Expired);
    }
    let subject = verified.claim_str("sub").ok_or(ExternalRefusal::Invalid)?;
    let (namespace, service_account) = subject
        .strip_prefix(SERVICE_ACCOUNT_SUBJECT_PREFIX)
        .and_then(|rest| rest.split_once(':'))
        .filter(|(namespace, name)| !namespace.is_empty() && !name.is_empty() && !name.contains(':'))
        .ok_or(ExternalRefusal::Invalid)?;
    let pod_name = verified
        .claims
        .get("kubernetes.io")
        .and_then(|kubernetes| kubernetes.get("pod"))
        .and_then(|pod| pod.get("name"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    Ok(ServiceAccountIdentity {
        namespace: namespace.to_string(),
        service_account: service_account.to_string(),
        pod_name,
    })
}

/// The EKS pod identity association binding `identity` in the cluster, with the IAM role's
/// id the session is reported under.
fn association<Eng: Send + Sync + 'static>(
    engine: &Eng,
    region: &str,
    cluster_name: &str,
    identity: &ServiceAccountIdentity,
) -> Option<PodIdentityAssociationRecord> {
    let eks = ::service_com_amazonaws_eks_awswesleyfrontend::state::service_state(engine)?;
    let record = eks
        .lock()
        .pod_identity_associations_in(region, cluster_name)
        .find(|record| record.namespace == identity.namespace && record.service_account == identity.service_account)
        .cloned()?;
    let role_name = record.role_arn.rsplit('/').next()?.to_string();
    let iam = ::service_com_amazonaws_iam_awsidentitymanagementv20100508::state::service_state(engine)?;
    let role_id = iam
        .lock()
        .roles
        .get(&::service_com_amazonaws_iam_awsidentitymanagementv20100508::state::name_key(&role_name))
        .filter(|role| role.arn == record.role_arn)?
        .role_id
        .clone();
    Some(PodIdentityAssociationRecord {
        association_id: record.association_id,
        association_arn: record.association_arn,
        namespace: record.namespace,
        service_account: record.service_account,
        role_arn: record.role_arn,
        role_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::ring::rand::SystemRandom;
    use ::ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
    use serde_json::{json, Value};
    use std::time::Duration;

    const ISSUER: &str = "https://oidc.eks.us-east-1.amazonaws.com/id/ABC";

    struct Signer {
        pair: EcdsaKeyPair,
        kid: &'static str,
    }

    impl Signer {
        fn new(kid: &'static str) -> Self {
            let rng = SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).expect("a key");
            let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).expect("a key pair");
            Self { pair, kid }
        }

        fn key_set(&self) -> Value {
            let point = self.pair.public_key().as_ref();
            json!({"keys": [{
                "kty": "EC", "crv": "P-256", "alg": "ES256", "use": "sig", "kid": self.kid,
                "x": jwt::encode_base64url(&point[1..33]),
                "y": jwt::encode_base64url(&point[33..65]),
            }]})
        }

        fn token(&self, claims: &Value) -> String {
            let header = json!({"alg": "ES256", "kid": self.kid, "typ": "JWT"});
            let signing_input = format!(
                "{}.{}",
                jwt::encode_base64url(header.to_string().as_bytes()),
                jwt::encode_base64url(claims.to_string().as_bytes())
            );
            let signature = self.pair.sign(&SystemRandom::new(), signing_input.as_bytes()).expect("a signature");
            format!("{signing_input}.{}", jwt::encode_base64url(signature.as_ref()))
        }
    }

    fn now() -> SystemTime {
        SystemTime::now()
    }

    fn seconds(at: SystemTime) -> u64 {
        at.duration_since(UNIX_EPOCH).expect("after the epoch").as_secs()
    }

    /// The claims a Kubernetes API server puts in a projected pod identity token.
    fn pod_token_claims() -> Value {
        let issued = seconds(now());
        json!({
            "iss": ISSUER,
            "aud": [PODS_AUDIENCE],
            "sub": "system:serviceaccount:kube-system:aws-load-balancer-controller",
            "iat": issued, "nbf": issued, "exp": issued + 3600,
            "kubernetes.io": {"namespace": "kube-system", "pod": {"name": "lbc-0", "uid": "u"}, "serviceaccount": {"name": "aws-load-balancer-controller", "uid": "v"}},
        })
    }

    fn with(mut claims: Value, name: &str, value: Value) -> Value {
        claims[name] = value;
        claims
    }

    #[test]
    fn a_token_the_cluster_signed_names_its_service_account_and_pod() {
        let signer = Signer::new("k1");

        let identity = verified_identity(&signer.token(&pod_token_claims()), &signer.key_set(), ISSUER, now());

        assert_eq!(
            identity,
            Ok(ServiceAccountIdentity {
                namespace: "kube-system".to_string(),
                service_account: "aws-load-balancer-controller".to_string(),
                pod_name: Some("lbc-0".to_string()),
            })
        );
    }

    #[test]
    fn a_token_signed_by_another_key_is_invalid() {
        let cluster = Signer::new("k1");
        let impostor = Signer::new("k1");

        let identity = verified_identity(&impostor.token(&pod_token_claims()), &cluster.key_set(), ISSUER, now());

        assert_eq!(identity, Err(ExternalRefusal::Invalid));
    }

    #[test]
    fn a_token_of_another_issuer_is_invalid() {
        let signer = Signer::new("k1");
        let claims = with(pod_token_claims(), "iss", json!("https://oidc.eks.us-east-1.amazonaws.com/id/OTHER"));

        assert_eq!(verified_identity(&signer.token(&claims), &signer.key_set(), ISSUER, now()), Err(ExternalRefusal::Invalid));
    }

    #[test]
    fn a_token_for_another_audience_is_invalid() {
        let signer = Signer::new("k1");
        let claims = with(pod_token_claims(), "aud", json!(["sts.amazonaws.com"]));

        assert_eq!(verified_identity(&signer.token(&claims), &signer.key_set(), ISSUER, now()), Err(ExternalRefusal::Invalid));
    }

    #[test]
    fn a_token_past_its_expiry_is_expired() {
        let signer = Signer::new("k1");
        let token = signer.token(&pod_token_claims());

        let later = now() + Duration::from_secs(3601);

        assert_eq!(verified_identity(&token, &signer.key_set(), ISSUER, later), Err(ExternalRefusal::Expired));
    }

    #[test]
    fn a_subject_that_is_not_a_service_account_is_invalid() {
        let signer = Signer::new("k1");
        for subject in ["admin", "system:serviceaccount:only-namespace", "system:serviceaccount::name", "system:serviceaccount:ns:a:b"] {
            let claims = with(pod_token_claims(), "sub", json!(subject));

            assert_eq!(
                verified_identity(&signer.token(&claims), &signer.key_set(), ISSUER, now()),
                Err(ExternalRefusal::Invalid),
                "{subject}"
            );
        }
    }
}
