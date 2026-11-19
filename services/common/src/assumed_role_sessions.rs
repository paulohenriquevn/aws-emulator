// SPDX-License-Identifier: CC0-1.0

//! fork: the role sessions a service other than STS issues credentials for, so STS can
//! report who a request signed with them is.
//!
//! EKS Auth (`AssumeRoleForPodIdentity`) hands a pod temporary credentials for an IAM
//! role, and AWS reports a request signed with them as that role's session:
//! `sts:GetCallerIdentity` answers `arn:aws:sts::<account>:assumed-role/<role>/<session>`.
//! STS keeps its own sessions in its own state, which EKS Auth cannot reach without
//! depending on the STS crate, so the issuing service records each session here, in the
//! shared engine (`engine.state::<AssumedRoleSessions>()`), and STS reads it for a key it
//! did not mint.

use std::collections::HashMap;
use std::time::SystemTime;

/// The most sessions the record holds, a bound of this module (not an AWS quota). A new
/// session past it first drops the expired ones, then the one that expires soonest.
pub const MAX_SESSIONS: usize = 100_000;

/// Who a request signed with a session's credentials is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssumedRoleSession {
    /// `arn:aws:sts::<account>:assumed-role/<role name>/<session name>`.
    pub arn: String,
    /// `<role id>:<session name>`, what `GetCallerIdentity` reports as `UserId`.
    pub user_id: String,
    pub account: String,
    /// When the credentials stop working.
    pub expiration: SystemTime,
}

impl AssumedRoleSession {
    /// The session `session_name` of the role `role_arn` (`arn:aws:iam::<account>:role/[path/]<name>`)
    /// whose id is `role_id`. `None` when `role_arn` is not a role ARN.
    pub fn of_role(role_arn: &str, role_id: &str, session_name: &str, expiration: SystemTime) -> Option<Self> {
        let rest = role_arn.strip_prefix("arn:")?;
        let mut fields = rest.splitn(5, ':');
        let (partition, service, _region, account, resource) =
            (fields.next()?, fields.next()?, fields.next()?, fields.next()?, fields.next()?);
        let role_name = resource.strip_prefix("role/")?.rsplit('/').next()?;
        if service != "iam" || account.is_empty() || role_name.is_empty() {
            return None;
        }
        Some(Self {
            arn: format!("arn:{partition}:sts::{account}:assumed-role/{role_name}/{session_name}"),
            user_id: format!("{role_id}:{session_name}"),
            account: account.to_string(),
            expiration,
        })
    }

    pub fn is_expired(&self, now: SystemTime) -> bool {
        now >= self.expiration
    }
}

/// The recorded sessions, by access key id.
#[derive(Debug, Default)]
pub struct AssumedRoleSessions {
    sessions: HashMap<String, AssumedRoleSession>,
}

impl AssumedRoleSessions {
    /// Record `session` as the identity of `access_key_id`, keeping the record within
    /// [`MAX_SESSIONS`].
    pub fn record(&mut self, access_key_id: &str, session: AssumedRoleSession, now: SystemTime) {
        if self.sessions.len() >= MAX_SESSIONS && !self.sessions.contains_key(access_key_id) {
            self.sessions.retain(|_, recorded| !recorded.is_expired(now));
            if self.sessions.len() >= MAX_SESSIONS {
                let soonest = self
                    .sessions
                    .iter()
                    .min_by_key(|(_, recorded)| recorded.expiration)
                    .map(|(key, _)| key.clone());
                if let Some(key) = soonest {
                    self.sessions.remove(&key);
                }
            }
        }
        self.sessions.insert(access_key_id.to_string(), session);
    }

    /// The session `access_key_id` belongs to, expired or not.
    pub fn session(&self, access_key_id: &str) -> Option<&AssumedRoleSession> {
        self.sessions.get(access_key_id)
    }

    /// Forget `access_key_id`'s session, once its credentials are deleted.
    pub fn forget(&mut self, access_key_id: &str) {
        self.sessions.remove(access_key_id);
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn a_role_session_is_named_as_sts_names_an_assumed_role() {
        let session = AssumedRoleSession::of_role("arn:aws:iam::000000000000:role/controllers/lbc", "AROAEXAMPLE", "eks-ops-pod-1", at(100));

        assert_eq!(
            session,
            Some(AssumedRoleSession {
                arn: "arn:aws:sts::000000000000:assumed-role/lbc/eks-ops-pod-1".to_string(),
                user_id: "AROAEXAMPLE:eks-ops-pod-1".to_string(),
                account: "000000000000".to_string(),
                expiration: at(100),
            })
        );
    }

    #[test]
    fn an_arn_that_is_not_a_role_names_no_session() {
        for arn in ["arn:aws:iam::000000000000:user/alice", "arn:aws:s3:::bucket", "role/lbc", "arn:aws:iam:::role/lbc"] {
            assert_eq!(AssumedRoleSession::of_role(arn, "AROA", "s", at(1)), None, "{arn}");
        }
    }

    #[test]
    fn a_recorded_session_is_found_by_its_key_until_forgotten() {
        let mut sessions = AssumedRoleSessions::default();
        let session = AssumedRoleSession::of_role("arn:aws:iam::000000000000:role/lbc", "AROA", "s", at(100)).expect("a role");

        sessions.record("ASIAKEY", session.clone(), at(0));
        assert_eq!(sessions.session("ASIAKEY"), Some(&session));

        sessions.forget("ASIAKEY");
        assert_eq!(sessions.session("ASIAKEY"), None);
    }

    #[test]
    fn a_session_is_expired_from_its_expiration_on() {
        let session = AssumedRoleSession::of_role("arn:aws:iam::000000000000:role/lbc", "AROA", "s", at(100)).expect("a role");

        assert!(!session.is_expired(at(99)));
        assert!(session.is_expired(at(100)));
    }
}
