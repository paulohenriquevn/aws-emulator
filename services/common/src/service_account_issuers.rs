// SPDX-License-Identifier: CC0-1.0

//! fork: the key sets of Kubernetes service account issuers whose control plane runs
//! OUTSIDE this emulator.
//!
//! An EKS cluster's API server signs the service account tokens its pods present to STS
//! (`AssumeRoleWithWebIdentity`, IRSA) and to EKS Auth (`AssumeRoleForPodIdentity`, EKS Pod
//! Identity), and a verifier checks them against the key set published at the cluster's
//! OIDC issuer URL. This emulator runs no Kubernetes API server, so a local cluster (kind,
//! k3d) plays that part: it signs with its own key, names the emulated EKS cluster's
//! issuer URL as `iss`, and its public key set is registered here under that URL. STS and
//! EKS Auth then verify such a token against this key set, exactly as AWS verifies a real
//! cluster's tokens against the key set at its issuer URL.
//!
//! Only public keys are held. The record lives in the shared engine as its own state type
//! (`engine.state::<ServiceAccountIssuerKeys>()`), so the services that read it share one
//! copy without depending on each other.

use std::collections::BTreeMap;

/// The most issuers the record holds, a bound of this module (not an AWS quota): each
/// registration replaces the key set of its issuer, and a new issuer past the bound is
/// refused.
pub const MAX_ISSUERS: usize = 1_000;

/// The most keys one registered key set may carry.
pub const MAX_KEYS_PER_ISSUER: usize = 20;

/// Why a key set was not registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySetDefect {
    /// The value is not a JSON object with a non-empty `keys` array of objects.
    NotAKeySet,
    /// More than [`MAX_KEYS_PER_ISSUER`] keys.
    TooManyKeys(usize),
    /// A new issuer would take the record past [`MAX_ISSUERS`].
    TooManyIssuers,
}

impl std::fmt::Display for KeySetDefect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAKeySet => f.write_str("the body is not a JSON Web Key Set: expected an object with a non-empty \"keys\" array of key objects"),
            Self::TooManyKeys(count) => write!(f, "the key set has {count} keys; at most {MAX_KEYS_PER_ISSUER} are accepted"),
            Self::TooManyIssuers => write!(f, "{MAX_ISSUERS} issuers are already registered"),
        }
    }
}

/// The registered key sets, by issuer URL.
#[derive(Debug, Default)]
pub struct ServiceAccountIssuerKeys {
    key_sets: BTreeMap<String, serde_json::Value>,
}

impl ServiceAccountIssuerKeys {
    /// Register `key_set` as what `issuer` publishes, replacing any earlier one.
    pub fn register(&mut self, issuer: &str, key_set: serde_json::Value) -> Result<(), KeySetDefect> {
        let keys = key_set
            .get("keys")
            .and_then(serde_json::Value::as_array)
            .filter(|keys| !keys.is_empty() && keys.iter().all(serde_json::Value::is_object))
            .ok_or(KeySetDefect::NotAKeySet)?;
        if keys.len() > MAX_KEYS_PER_ISSUER {
            return Err(KeySetDefect::TooManyKeys(keys.len()));
        }
        let issuer = normalized(issuer);
        if !self.key_sets.contains_key(&issuer) && self.key_sets.len() >= MAX_ISSUERS {
            return Err(KeySetDefect::TooManyIssuers);
        }
        self.key_sets.insert(issuer, key_set);
        Ok(())
    }

    /// The key set registered for `issuer`, if any.
    #[must_use]
    pub fn key_set(&self, issuer: &str) -> Option<&serde_json::Value> {
        self.key_sets.get(&normalized(issuer))
    }
}

/// An issuer URL as the record keys it: without a trailing slash, which an `iss` claim and
/// an issuer URL may differ by.
fn normalized(issuer: &str) -> String {
    issuer.trim().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ISSUER: &str = "https://oidc.eks.us-east-1.amazonaws.com/id/ABC";

    fn key_set() -> serde_json::Value {
        json!({"keys": [{"kty": "RSA", "kid": "k1", "n": "AQAB", "e": "AQAB"}]})
    }

    #[test]
    fn a_registered_key_set_is_found_with_or_without_a_trailing_slash() {
        let mut issuers = ServiceAccountIssuerKeys::default();

        issuers.register(ISSUER, key_set()).expect("a key set");

        assert_eq!(issuers.key_set(ISSUER), Some(&key_set()));
        assert_eq!(issuers.key_set(&format!("{ISSUER}/")), Some(&key_set()));
    }

    #[test]
    fn an_unregistered_issuer_has_no_key_set() {
        let issuers = ServiceAccountIssuerKeys::default();

        assert_eq!(issuers.key_set(ISSUER), None);
    }

    #[test]
    fn a_second_registration_replaces_the_first() {
        let mut issuers = ServiceAccountIssuerKeys::default();
        issuers.register(ISSUER, key_set()).expect("a key set");
        let rotated = json!({"keys": [{"kty": "RSA", "kid": "k2", "n": "AQAB", "e": "AQAB"}]});

        issuers.register(ISSUER, rotated.clone()).expect("a key set");

        assert_eq!(issuers.key_set(ISSUER), Some(&rotated));
    }

    #[test]
    fn a_value_that_is_not_a_key_set_is_refused() {
        let mut issuers = ServiceAccountIssuerKeys::default();

        for value in [json!({}), json!({"keys": []}), json!({"keys": ["x"]}), json!([])] {
            assert_eq!(issuers.register(ISSUER, value.clone()), Err(KeySetDefect::NotAKeySet), "{value}");
        }
        assert_eq!(issuers.key_set(ISSUER), None);
    }

    #[test]
    fn a_key_set_with_too_many_keys_is_refused() {
        let mut issuers = ServiceAccountIssuerKeys::default();
        let keys: Vec<_> = (0..=MAX_KEYS_PER_ISSUER).map(|index| json!({"kid": index})).collect();

        let refused = issuers.register(ISSUER, json!({ "keys": keys }));

        assert_eq!(refused, Err(KeySetDefect::TooManyKeys(MAX_KEYS_PER_ISSUER + 1)));
    }
}
