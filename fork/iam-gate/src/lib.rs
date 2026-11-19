// SPDX-License-Identifier: CC0-1.0
#![forbid(unsafe_code)]
//! IAM for every service the fork mounts, with `EMULATOR_IAM_ENFORCE=on`.
//!
//! Upstream asks IAM only from a few services (IAM itself, Secrets Manager, ACM and part of
//! KMS); every other service serves any signed call. This gate runs in the router after
//! request authentication and asks the caller's identity-based policies whether they allow
//! `<service prefix>:<operation>`.
//!
//! What is simulated: the router does not know which resource a request addresses or the
//! values its condition keys would take, so a decision is made on the ACTION alone. An
//! `Allow` statement counts for every action it names, whatever its `Resource`,
//! `NotResource` and `Condition` say. A `Deny` statement counts only when it applies to
//! every resource (`"Resource": "*"`) without a condition. This catches an action missing
//! from a role's policy, which is the mistake a staging environment should surface, without
//! refusing a call a scoped grant would allow on AWS. A permissions boundary is read the
//! same way.
//!
//! Who is not asked about: the account's own published credential (the emulator's
//! administrator), a request carrying no SigV4 `Authorization` header, a principal holding
//! an AWS managed policy this emulator does not carry (its grant cannot be read, as in
//! Secrets Manager's own gate), and the services that authorize their calls themselves:
//! IAM, STS, Secrets Manager and ACM.

use std::sync::Arc;
use std::time::SystemTime;

use ::emulator_server::{CredentialStore, Denied};
use ::service_com_amazonaws_iam_awsidentitymanagementv20100508::state as iam_store;
use ::service_com_amazonaws_sts_awssecuritytokenservicev20110615::state as sts_store;
use ::service_common::iam_authorization::{self as iam, Decision};
use serde_json::Value;

/// The variable that turns the gate on.
pub const ENFORCE_ENV: &str = "EMULATOR_IAM_ENFORCE";

/// The signing names of the services that authorize their own calls.
const SELF_AUTHORIZING: [&str; 4] = ["iam", "sts", "secretsmanager", "acm"];

/// Whether `value` (the variable's value) turns the gate on.
pub fn enforce_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "on" | "1" | "true"))
}

/// The gate, over the engine whose IAM and STS stores it reads.
pub struct IamGate {
    engine: Arc<engine::InMemoryEngine>,
}

impl IamGate {
    pub fn new(engine: Arc<engine::InMemoryEngine>) -> Self {
        Self { engine }
    }
}

/// Install the gate on `router`, reading the IAM and STS stores of `engine`.
pub fn install(router: &mut ::smithy_server_runtime::MultiServiceRouter, engine: Arc<engine::InMemoryEngine>) {
    router.set_request_authorizer(Arc::new(IamGate::new(engine)));
}

const ACCESS_DENIED: Denied = Denied {
    code: "AccessDenied",
    message: "User is not authorized to perform this action because no identity-based policy allows it (EMULATOR_IAM_ENFORCE)",
};
const ACCESS_DENIED_EXCEPTION: Denied = Denied {
    code: "AccessDeniedException",
    message: ACCESS_DENIED.message,
};
const UNAUTHORIZED_OPERATION: Denied = Denied {
    code: "UnauthorizedOperation",
    message: "You are not authorized to perform this operation (EMULATOR_IAM_ENFORCE)",
};

/// The error code a refusal carries in the service's protocol: EC2's own
/// `UnauthorizedOperation`, `AccessDeniedException` for the JSON protocols, and
/// `AccessDenied` for the query and XML ones.
fn refusal(signing_name: &str) -> Denied {
    match signing_name {
        "ec2" => UNAUTHORIZED_OPERATION,
        "ecr" | "ssm" | "logs" | "events" | "kms" | "eks" | "eks-auth" | "sqs" | "wafv2" | "shield"
        | "budgets" | "pricing" => ACCESS_DENIED_EXCEPTION,
        _ => ACCESS_DENIED,
    }
}

/// The IAM action a request asks for: the service prefix and the action the operation maps
/// to. The prefix is the signing name, except where IAM names the service otherwise; an S3
/// operation is mapped where the action IAM checks for it has another name.
pub fn action_of(signing_name: &str, operation: &str) -> String {
    let prefix = match signing_name {
        "monitoring" => "cloudwatch",
        other => other,
    };
    let action = if prefix == "s3" { s3_action(operation) } else { operation };
    format!("{prefix}:{action}")
}

/// The action IAM checks for an S3 operation whose action is named otherwise (Amazon S3 User
/// Guide, "Policy actions for Amazon S3").
fn s3_action(operation: &str) -> &str {
    match operation {
        "ListObjects" | "ListObjectsV2" | "HeadBucket" => "ListBucket",
        "ListObjectVersions" => "ListBucketVersions",
        "ListBuckets" => "ListAllMyBuckets",
        "HeadObject" | "GetObjectAttributes" => "GetObject",
        "DeleteObjects" => "DeleteObject",
        "CopyObject" | "CreateMultipartUpload" | "UploadPart" | "UploadPartCopy" | "CompleteMultipartUpload" => {
            "PutObject"
        }
        "AbortMultipartUpload" => "AbortMultipartUpload",
        "ListMultipartUploads" => "ListBucketMultipartUploads",
        "ListParts" => "ListMultipartUploadParts",
        "GetBucketLifecycleConfiguration" => "GetLifecycleConfiguration",
        "PutBucketLifecycleConfiguration" | "DeleteBucketLifecycle" => "PutLifecycleConfiguration",
        "GetBucketCors" => "GetBucketCORS",
        "PutBucketCors" | "DeleteBucketCors" => "PutBucketCORS",
        "GetBucketEncryption" => "GetEncryptionConfiguration",
        "PutBucketEncryption" | "DeleteBucketEncryption" => "PutEncryptionConfiguration",
        "GetPublicAccessBlock" => "GetBucketPublicAccessBlock",
        "PutPublicAccessBlock" | "DeletePublicAccessBlock" => "PutBucketPublicAccessBlock",
        "GetBucketReplication" => "GetReplicationConfiguration",
        "PutBucketReplication" | "DeleteBucketReplication" => "PutReplicationConfiguration",
        "GetObjectLockConfiguration" => "GetBucketObjectLockConfiguration",
        "PutObjectLockConfiguration" => "PutBucketObjectLockConfiguration",
        "GetBucketAccelerateConfiguration" => "GetAccelerateConfiguration",
        "PutBucketAccelerateConfiguration" => "PutAccelerateConfiguration",
        "GetBucketNotificationConfiguration" => "GetBucketNotification",
        "PutBucketNotificationConfiguration" => "PutBucketNotification",
        "DeleteBucketTagging" => "PutBucketTagging",
        "DeleteBucketWebsite" => "DeleteBucketWebsite",
        "DeleteBucketOwnershipControls" => "PutBucketOwnershipControls",
        other => other,
    }
}

/// `document` read at the level of the action (see the module docs): every `Allow` applies
/// to every resource without conditions, and a `Deny` stays only when it already did. An
/// unreadable document is answered as it is, so the evaluator reads it as it would.
pub fn action_level(document: &str) -> String {
    let Ok(mut parsed) = serde_json::from_str::<Value>(document) else {
        return document.to_string();
    };
    let Some(statements) = parsed.get_mut("Statement") else {
        return document.to_string();
    };
    let list: Vec<Value> = match statements.take() {
        Value::Array(list) => list,
        single => vec![single],
    };
    let kept: Vec<Value> = list
        .into_iter()
        .filter_map(|mut statement| {
            let object = statement.as_object_mut()?;
            match object.get("Effect").and_then(Value::as_str) {
                Some("Allow") => {
                    object.remove("NotResource");
                    object.remove("Condition");
                    object.insert("Resource".to_string(), Value::String("*".to_string()));
                    Some(statement)
                }
                Some("Deny") => {
                    let everywhere = match object.get("Resource") {
                        Some(Value::String(resource)) => resource == "*",
                        Some(Value::Array(resources)) => resources.iter().any(|resource| resource == "*"),
                        _ => false,
                    };
                    (everywhere && !object.contains_key("Condition") && !object.contains_key("NotResource"))
                        .then_some(statement)
                }
                _ => Some(statement),
            }
        })
        .collect();
    *statements = Value::Array(kept);
    parsed.to_string()
}

/// The principal a request speaks for and the documents in force on it.
struct Caller {
    arn: String,
    principals: Vec<String>,
    documents: Vec<String>,
    boundary: Option<String>,
    /// The account's own published credential.
    administrator: bool,
    /// A grant this emulator cannot read (an AWS managed policy it does not carry).
    unreadable_attachment: bool,
}

impl IamGate {
    /// The caller the access key id speaks for, read as Secrets Manager's own gate reads it:
    /// an STS session, else the IAM user owning a long-term key, else the account.
    fn caller(&self, access_key_id: &str, credential_store: &CredentialStore) -> Option<Caller> {
        let identity = match sts_store::service_state(&*self.engine) {
            Some(sts) => sts_store::resolve_caller(&mut sts.lock(), access_key_id, SystemTime::now(), credential_store)
                .ok()?,
            None => sts_store::ambient_identity(),
        };
        let mut caller = Caller {
            administrator: identity.arn == sts_store::ambient_identity().arn,
            principals: vec![identity.arn.clone()],
            arn: identity.arn,
            documents: Vec::new(),
            boundary: None,
            unreadable_attachment: false,
        };
        let Some(iam) = iam_store::service_state(&*self.engine) else {
            return Some(caller);
        };
        let store = iam.lock();
        if caller.administrator {
            if let Some(user) = store
                .access_keys
                .get(access_key_id)
                .and_then(|record| store.users.get(&record.user_key))
            {
                caller.administrator = false;
                caller.arn = user.arn.clone();
                caller.principals = vec![user.arn.clone()];
            }
        }
        let identity_arn = if let Some(role_name) = ::service_common::iam_role::assumed_role_session_role_name(&caller.arn) {
            store.roles.get(&iam_store::name_key(role_name)).map(|held| held.arn.clone())
        } else if ::service_common::iam_role::user_name(&caller.arn).is_some() {
            Some(caller.arn.clone())
        } else {
            None
        };
        if let Some(identity_arn) = identity_arn {
            if identity_arn != caller.arn {
                caller.principals.push(identity_arn.clone());
            }
            if let Some(policies) = store.principal_policies(&identity_arn) {
                caller.documents = policies.documents;
                caller.unreadable_attachment = !policies.uncarried.is_empty();
            }
            caller.boundary = store.permissions_boundary_document(&identity_arn);
        }
        Some(caller)
    }
}

/// Whether documents read at the level of the action allow `action` for `caller`.
fn decide(caller: &Caller, action: &str) -> Result<(), String> {
    if caller.administrator {
        return Ok(());
    }
    let documents: Vec<String> = caller.documents.iter().map(|document| action_level(document)).collect();
    let request = iam::Request::new(action, "*", &caller.principals);
    let said = iam::identity_decision(&documents, &request);
    if said == Decision::Denied {
        return Err("an explicit deny in an identity-based policy".to_string());
    }
    if !(iam::is_allowed(said, Decision::NotStated) || caller.unreadable_attachment) {
        return Err("no identity-based policy allows it".to_string());
    }
    if let Some(boundary) = &caller.boundary {
        if iam::identity_decision(&[action_level(boundary)], &request) != Decision::Allowed {
            return Err("the permissions boundary does not allow it".to_string());
        }
    }
    Ok(())
}

impl ::smithy_server_runtime::RequestAuthorizer for IamGate {
    fn authorize(
        &self,
        parts: &http::request::Parts,
        credential_store: &CredentialStore,
        signing_name: &str,
        operation: &str,
    ) -> Result<(), Denied> {
        if SELF_AUTHORIZING.contains(&signing_name) {
            return Ok(());
        }
        let access_key_id = ::engine::extract_akid(&parts.headers);
        if access_key_id.is_empty() {
            return Ok(());
        }
        let action = action_of(signing_name, operation);
        let Some(caller) = self.caller(&access_key_id, credential_store) else {
            tracing::warn!(%action, "refused: the session credential has expired");
            return Err(refusal(signing_name));
        };
        decide(&caller, &action).map_err(|reason| {
            tracing::warn!(principal = %caller.arn, %action, %reason, "refused by EMULATOR_IAM_ENFORCE");
            refusal(signing_name)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(documents: &[&str]) -> Caller {
        Caller {
            arn: "arn:aws:sts::000000000000:assumed-role/app/session".to_string(),
            principals: vec!["arn:aws:sts::000000000000:assumed-role/app/session".to_string()],
            documents: documents.iter().map(|document| document.to_string()).collect(),
            boundary: None,
            administrator: false,
            unreadable_attachment: false,
        }
    }

    #[test]
    fn the_action_is_the_signing_name_and_the_operation() {
        assert_eq!(action_of("rds", "CreateDBInstance"), "rds:CreateDBInstance");
        assert_eq!(action_of("elasticloadbalancing", "DescribeTargetGroups"), "elasticloadbalancing:DescribeTargetGroups");
    }

    #[test]
    fn cloudwatch_is_named_by_its_iam_prefix() {
        assert_eq!(action_of("monitoring", "PutMetricData"), "cloudwatch:PutMetricData");
    }

    #[test]
    fn an_s3_operation_maps_to_the_action_iam_checks() {
        assert_eq!(action_of("s3", "ListObjectsV2"), "s3:ListBucket");
        assert_eq!(action_of("s3", "HeadObject"), "s3:GetObject");
        assert_eq!(action_of("s3", "PutObject"), "s3:PutObject");
    }

    #[test]
    fn a_role_without_the_action_is_refused() {
        let caller = role(&[r#"{"Statement":[{"Effect":"Allow","Action":"rds:Describe*","Resource":"*"}]}"#]);
        assert!(decide(&caller, "rds:CreateDBInstance").is_err());
    }

    #[test]
    fn a_role_with_the_action_is_allowed() {
        let caller = role(&[r#"{"Statement":[{"Effect":"Allow","Action":["rds:CreateDBInstance"],"Resource":"*"}]}"#]);
        assert!(decide(&caller, "rds:CreateDBInstance").is_ok());
    }

    #[test]
    fn a_grant_scoped_by_resource_and_condition_counts_for_its_action() {
        let caller = role(&[r#"{"Statement":{"Effect":"Allow","Action":"ec2:CreateTags","Resource":"arn:aws:ec2:*:*:security-group/*","Condition":{"Null":{"aws:RequestTag/elbv2.k8s.aws/cluster":"false"}}}}"#]);
        assert!(decide(&caller, "ec2:CreateTags").is_ok());
    }

    #[test]
    fn an_unconditional_deny_on_every_resource_wins_over_an_allow() {
        let caller = role(&[r#"{"Statement":[{"Effect":"Allow","Action":"*","Resource":"*"},{"Effect":"Deny","Action":"ecr:*","Resource":"*"}]}"#]);
        assert!(decide(&caller, "ecr:DescribeRepositories").is_err());
    }

    #[test]
    fn a_scoped_deny_is_not_applied_without_its_resource() {
        let caller = role(&[r#"{"Statement":[{"Effect":"Allow","Action":"ec2:*","Resource":"*"},{"Effect":"Deny","Action":"ec2:DeleteTags","Resource":"*","Condition":{"Null":{"aws:ResourceTag/x":"true"}}}]}"#]);
        assert!(decide(&caller, "ec2:DeleteTags").is_ok());
    }

    #[test]
    fn the_administrator_needs_no_policy() {
        let mut caller = role(&[]);
        caller.administrator = true;
        assert!(decide(&caller, "rds:CreateDBInstance").is_ok());
    }

    #[test]
    fn a_boundary_without_the_action_refuses_it() {
        let mut caller = role(&[r#"{"Statement":[{"Effect":"Allow","Action":"*","Resource":"*"}]}"#]);
        caller.boundary = Some(r#"{"Statement":[{"Effect":"Allow","Action":"s3:*","Resource":"*"}]}"#.to_string());
        assert!(decide(&caller, "rds:CreateDBInstance").is_err());
        assert!(decide(&caller, "s3:GetObject").is_ok());
    }

    #[test]
    fn only_on_and_its_spellings_turn_the_gate_on() {
        assert!(enforce_enabled(Some("on")));
        assert!(enforce_enabled(Some("1")));
        assert!(!enforce_enabled(Some("off")));
        assert!(!enforce_enabled(None));
    }
}
