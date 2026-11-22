// SPDX-License-Identifier: CC0-1.0

//! `DescribeTags` reports the tags `CreateTags` wrote, one `TagDescription` per tag.
//!
//! The Terraform AWS provider reads an `aws_ec2_tag` back with `DescribeTags` right after
//! it creates one, and `terraform-aws-modules/eks` tags the cluster's primary security
//! group that way, so a 501 here fails the apply of every EKS cluster. The rules under
//! test:
//!
//! 1. a tag written by `CreateTags` reads back with its `Key`, `Value`, `ResourceId` and the
//!    `ResourceType` a `TagSpecification` names that kind of resource by;
//! 2. the `key`, `value`, `resource-id`, `resource-type` and `tag:<key>` filters select
//!    tags, the values of one filter joined with OR and the filters with AND, and a filter
//!    name the operation does not document is refused;
//! 3. `MaxResults` outside 5..=1000 is refused with `InvalidParameterValue`, and `NextToken`
//!    walks every tag exactly once;
//! 4. `DeleteTags` removes the tag from the report, a resource of another Region is not
//!    reported, and `DryRun` answers `DryRunOperation`.

use std::collections::BTreeSet;

use smithy_server_runtime::MultiServiceRouter;

use super::ec2_query::{call, element, emulator, error_code, filters};

const REGION: &str = "us-west-2";

/// An AMI id every launch in this crate's suites uses; the published catalog serves it.
const AMI: &str = "ami-0123456789abcdef0";

fn p(key: &str, value: &str) -> (String, String) {
    (key.to_string(), value.to_string())
}

/// One `TagDescription` as the response carries it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Described {
    resource_id: String,
    key: String,
    value: String,
    resource_type: String,
}

async fn ok(router: &MultiServiceRouter, action: &str, params: &[(String, String)]) -> String {
    let (status, body) = call(router, REGION, action, params).await;
    assert_eq!(status, 200, "{action} must succeed, got {status}: {body}");
    body
}

async fn create_vpc(router: &MultiServiceRouter, cidr: &str) -> String {
    let body = ok(router, "CreateVpc", &[p("CidrBlock", cidr)]).await;
    element(&body, "vpcId").unwrap_or_else(|| panic!("CreateVpc answers a vpcId: {body}"))
}

async fn tag(router: &MultiServiceRouter, resource_id: &str, tags: &[(&str, &str)]) {
    let mut params = vec![p("ResourceId.1", resource_id)];
    for (index, (key, value)) in tags.iter().enumerate() {
        params.push(p(&format!("Tag.{}.Key", index + 1), key));
        params.push(p(&format!("Tag.{}.Value", index + 1), value));
    }
    ok(router, "CreateTags", &params).await;
}

/// The `TagDescription`s one response body carries, in the order it carries them: the
/// `<item>` children of the envelope's own `<tagSet>`.
fn described(body: &str) -> Vec<Described> {
    let document = roxmltree::Document::parse(body)
        .unwrap_or_else(|error| panic!("the response is not XML ({error}): {body}"));
    let child_text = |node: roxmltree::Node<'_, '_>, name: &str| {
        node.children()
            .find(|child| child.is_element() && child.tag_name().name() == name)
            .and_then(|child| child.text())
            .unwrap_or_default()
            .to_string()
    };
    let Some(set) = document
        .root_element()
        .children()
        .find(|node| node.is_element() && node.tag_name().name() == "tagSet")
    else {
        return Vec::new();
    };
    set.children()
        .filter(|node| node.is_element() && node.tag_name().name() == "item")
        .map(|item| Described {
            resource_id: child_text(item, "resourceId"),
            key: child_text(item, "key"),
            value: child_text(item, "value"),
            resource_type: child_text(item, "resourceType"),
        })
        .collect()
}

async fn describe(router: &MultiServiceRouter, params: &[(String, String)]) -> Vec<Described> {
    described(&ok(router, "DescribeTags", params).await)
}

fn row(resource_id: &str, key: &str, value: &str, resource_type: &str) -> Described {
    Described {
        resource_id: resource_id.to_string(),
        key: key.to_string(),
        value: value.to_string(),
        resource_type: resource_type.to_string(),
    }
}

#[tokio::test]
async fn a_tag_written_by_create_tags_reads_back_by_resource_id() {
    let router = emulator().0;
    let vpc_id = create_vpc(&router, "10.0.0.0/16").await;
    tag(&router, &vpc_id, &[("team", "core")]).await;

    let rows = describe(&router, &filters(&[("resource-id", &[&vpc_id])])).await;
    assert_eq!(rows, vec![row(&vpc_id, "team", "core", "vpc")]);
}

#[tokio::test]
async fn each_kind_of_resource_reports_its_tag_specification_resource_type() {
    let router = emulator().0;
    let vpc_id = create_vpc(&router, "10.1.0.0/16").await;
    let subnet_body = ok(
        &router,
        "CreateSubnet",
        &[p("VpcId", &vpc_id), p("CidrBlock", "10.1.1.0/24")],
    )
    .await;
    let subnet_id = element(&subnet_body, "subnetId").expect("CreateSubnet answers a subnetId");
    let group_body = ok(
        &router,
        "CreateSecurityGroup",
        &[
            p("GroupName", "cluster"),
            p("GroupDescription", "cluster primary"),
            p("VpcId", &vpc_id),
        ],
    )
    .await;
    let group_id = element(&group_body, "groupId").expect("CreateSecurityGroup answers a groupId");
    let interface_body = ok(&router, "CreateNetworkInterface", &[p("SubnetId", &subnet_id)]).await;
    let interface_id = element(&interface_body, "networkInterfaceId")
        .expect("CreateNetworkInterface answers a networkInterfaceId");
    let acl_body = ok(&router, "CreateNetworkAcl", &[p("VpcId", &vpc_id)]).await;
    let acl_id = element(&acl_body, "networkAclId").expect("CreateNetworkAcl answers an id");
    let route_table_body = ok(&router, "CreateRouteTable", &[p("VpcId", &vpc_id)]).await;
    let route_table_id =
        element(&route_table_body, "routeTableId").expect("CreateRouteTable answers an id");
    let launch_body = ok(
        &router,
        "RunInstances",
        &[p("ImageId", AMI), p("MinCount", "1"), p("MaxCount", "1")],
    )
    .await;
    let instance_id = element(&launch_body, "instanceId").expect("RunInstances answers an id");

    let expected = [
        (&vpc_id, "vpc"),
        (&subnet_id, "subnet"),
        (&group_id, "security-group"),
        (&interface_id, "network-interface"),
        (&acl_id, "network-acl"),
        (&route_table_id, "route-table"),
        (&instance_id, "instance"),
    ];
    for (resource_id, _) in &expected {
        tag(&router, resource_id, &[("kind-check", "yes")]).await;
    }

    let rows = describe(&router, &filters(&[("key", &["kind-check"])])).await;
    let reported: BTreeSet<(String, String)> = rows
        .iter()
        .map(|row| (row.resource_id.clone(), row.resource_type.clone()))
        .collect();
    let wanted: BTreeSet<(String, String)> = expected
        .iter()
        .map(|(resource_id, resource_type)| ((*resource_id).clone(), (*resource_type).to_string()))
        .collect();
    assert_eq!(reported, wanted, "every tagged kind is reported once with its type: {rows:?}");
}

#[tokio::test]
async fn filter_values_are_alternatives_while_every_filter_must_hold() {
    let router = emulator().0;
    let first = create_vpc(&router, "10.2.0.0/16").await;
    let second = create_vpc(&router, "10.3.0.0/16").await;
    tag(&router, &first, &[("team", "core"), ("env", "staging")]).await;
    tag(&router, &second, &[("team", "data"), ("env", "prod")]).await;

    // `key`: every tag of that key, whatever resource carries it.
    let rows = describe(&router, &filters(&[("key", &["team"])])).await;
    assert_eq!(
        rows.into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([row(&first, "team", "core", "vpc"), row(&second, "team", "data", "vpc")])
    );

    // `value`, two values joined with OR.
    let rows = describe(&router, &filters(&[("value", &["core", "prod"])])).await;
    assert_eq!(
        rows.into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([row(&first, "team", "core", "vpc"), row(&second, "env", "prod", "vpc")])
    );

    // `tag:<key>` selects the RESOURCES holding that pair and reports every tag they carry.
    let rows = describe(&router, &filters(&[("tag:team", &["core"])])).await;
    assert_eq!(
        rows.into_iter().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            row(&first, "env", "staging", "vpc"),
            row(&first, "team", "core", "vpc"),
        ])
    );

    // Filters joined with AND: one resource, one key.
    let rows = describe(
        &router,
        &filters(&[
            ("resource-id", &[&first, &second]),
            ("resource-type", &["vpc"]),
            ("key", &["env"]),
            ("tag:team", &["data"]),
        ]),
    )
    .await;
    assert_eq!(rows, vec![row(&second, "env", "prod", "vpc")]);

    // A resource type nothing tagged carries selects nothing.
    let rows = describe(&router, &filters(&[("resource-type", &["subnet"]), ("key", &["team"])]))
        .await;
    assert_eq!(rows, Vec::new());
}

#[tokio::test]
async fn an_undocumented_filter_name_is_refused() {
    let router = emulator().0;
    let (status, body) =
        call(&router, REGION, "DescribeTags", &filters(&[("owner-id", &["123456789012"])])).await;
    assert_eq!(status, 400, "an unknown filter must be refused, got {status}: {body}");
    assert_eq!(error_code(&body), "InvalidParameterValue", "{body}");
}

#[tokio::test]
async fn max_results_outside_five_to_one_thousand_is_refused() {
    let router = emulator().0;
    for max_results in ["4", "1001"] {
        let (status, body) =
            call(&router, REGION, "DescribeTags", &[p("MaxResults", max_results)]).await;
        assert_eq!(status, 400, "MaxResults={max_results} must be refused, got {status}: {body}");
        assert_eq!(error_code(&body), "InvalidParameterValue", "MaxResults={max_results}: {body}");
    }
    for max_results in ["5", "1000"] {
        ok(&router, "DescribeTags", &[p("MaxResults", max_results)]).await;
    }
}

#[tokio::test]
async fn next_token_walks_every_tag_exactly_once() {
    let router = emulator().0;
    let first = create_vpc(&router, "10.4.0.0/16").await;
    let second = create_vpc(&router, "10.5.0.0/16").await;
    tag(&router, &first, &[("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")]).await;
    tag(&router, &second, &[("a", "5"), ("b", "6"), ("c", "7")]).await;

    let mut walked: Vec<Described> = Vec::new();
    let mut page_sizes: Vec<usize> = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut params = vec![p("MaxResults", "5")];
        if let Some(token) = &token {
            params.push(p("NextToken", token));
        }
        let body = ok(&router, "DescribeTags", &params).await;
        let rows = described(&body);
        page_sizes.push(rows.len());
        walked.extend(rows);
        token = element(&body, "nextToken");
        if token.is_none() {
            break;
        }
        assert!(page_sizes.len() < 10, "the walk must end: {page_sizes:?}");
    }
    assert_eq!(page_sizes, vec![5, 2], "seven tags in pages of five");

    let unique: BTreeSet<Described> = walked.iter().cloned().collect();
    assert_eq!(unique.len(), walked.len(), "no tag is reported twice: {walked:?}");
    let unpaged = describe(&router, &[]).await;
    assert_eq!(walked, unpaged, "the walk reports what one unpaged call does, in its order");
}

#[tokio::test]
async fn a_next_token_this_operation_did_not_mint_is_refused() {
    let router = emulator().0;
    let (status, body) =
        call(&router, REGION, "DescribeTags", &[p("NextToken", "not-a-token")]).await;
    assert_eq!(status, 400, "a token this operation did not mint is refused: {body}");
    assert_eq!(error_code(&body), "InvalidPaginationToken", "{body}");
}

#[tokio::test]
async fn delete_tags_removes_the_tag_from_the_report() {
    let router = emulator().0;
    let vpc_id = create_vpc(&router, "10.6.0.0/16").await;
    tag(&router, &vpc_id, &[("team", "core"), ("env", "staging")]).await;
    ok(&router, "DeleteTags", &[p("ResourceId.1", &vpc_id), p("Tag.1.Key", "team")]).await;

    let rows = describe(&router, &filters(&[("resource-id", &[&vpc_id])])).await;
    assert_eq!(rows, vec![row(&vpc_id, "env", "staging", "vpc")]);
}

#[tokio::test]
async fn a_resource_of_another_region_is_not_reported() {
    let router = emulator().0;
    let vpc_id = create_vpc(&router, "10.7.0.0/16").await;
    tag(&router, &vpc_id, &[("team", "core")]).await;

    let (status, body) = call(
        &router,
        "us-east-1",
        "DescribeTags",
        &filters(&[("resource-id", &[&vpc_id])]),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(described(&body), Vec::new(), "a us-west-2 VPC is not a us-east-1 resource");
}

#[tokio::test]
async fn dry_run_answers_dry_run_operation() {
    let router = emulator().0;
    let (status, body) = call(&router, REGION, "DescribeTags", &[p("DryRun", "true")]).await;
    assert_eq!(status, 412, "{body}");
    assert_eq!(error_code(&body), "DryRunOperation", "{body}");
}
