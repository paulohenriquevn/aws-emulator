// SPDX-License-Identifier: CC0-1.0

//! A wildcard name and its apex share one DNS validation CNAME.
//!
//! ACM proves `*.example.org` by proving `example.org`, so it strips the `*.` before it
//! mints the record: both names report the SAME `ResourceRecord`, and every other name
//! gets its own (ACM User Guide, "DNS validation"). Clients rely on it:
//! `terraform-aws-modules/acm` publishes one record per distinct name with the wildcard
//! label removed, and `aws_acm_certificate_validation` then waits for the record of every
//! name, the apex included. Two different records for the pair leave one unpublished and
//! the validation can never finish.

use serde_json::json;

use crate::certificate_lifecycle::{describe, request_certificate, router};

const APEX: &str = "wildcard-probe-theo.com";

fn resource_record_of(options: &serde_json::Value, domain: &str) -> serde_json::Value {
    options
        .as_array()
        .unwrap_or_else(|| panic!("no DomainValidationOptions in {options}"))
        .iter()
        .find(|option| option["DomainName"] == domain)
        .unwrap_or_else(|| panic!("no validation option for {domain} in {options}"))["ResourceRecord"]
        .clone()
}

#[tokio::test]
async fn a_wildcard_reports_the_same_validation_record_as_its_apex() {
    let router = router();
    let wildcard = format!("*.{APEX}");
    let www = format!("www.{APEX}");
    let arn = request_certificate(
        &router,
        &json!({
            "DomainName": APEX,
            "ValidationMethod": "DNS",
            "SubjectAlternativeNames": [wildcard, www],
        }),
    )
    .await;

    let options = describe(&router, &arn).await["DomainValidationOptions"].clone();

    let apex_record = resource_record_of(&options, APEX);
    assert!(apex_record.is_object(), "the apex must carry a ResourceRecord: {options}");
    assert_eq!(
        resource_record_of(&options, &wildcard),
        apex_record,
        "the wildcard must report its apex's validation record"
    );
}

#[tokio::test]
async fn a_name_other_than_the_apex_keeps_its_own_validation_record() {
    let router = router();
    let www = format!("www.{APEX}");
    let arn = request_certificate(
        &router,
        &json!({
            "DomainName": APEX,
            "ValidationMethod": "DNS",
            "SubjectAlternativeNames": [format!("*.{APEX}"), www],
        }),
    )
    .await;

    let options = describe(&router, &arn).await["DomainValidationOptions"].clone();

    let www_record = resource_record_of(&options, &www);
    let apex_record = resource_record_of(&options, APEX);
    assert_ne!(www_record["Name"], apex_record["Name"], "www must get its own record name: {options}");
    assert_ne!(www_record["Value"], apex_record["Value"], "www must get its own record value: {options}");
}
