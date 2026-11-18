// SPDX-License-Identifier: CC0-1.0

//! `CreateTags` and `DeleteTags` act on network ACLs, the default one a VPC comes with
//! included.
//!
//! The Terraform AWS provider tags a VPC's default network ACL right after it adopts it
//! (`aws_default_network_acl`), so a refusal here fails every configuration that manages
//! one, `terraform-aws-modules/vpc` with its default settings among them. The rules under
//! test:
//!
//! 1. an ACL of either kind (the default one `CreateVpc` writes, and one `CreateNetworkAcl`
//!    creates) accepts `CreateTags`, and its tags read back through `DescribeNetworkAcls`;
//! 2. `DeleteTags` removes the tag again;
//! 3. an ACL id that names nothing is still refused with `InvalidNetworkAclID.NotFound`.

use smithy_server_runtime::MultiServiceRouter;

use super::ec2_query::{call, element, error_code};

const REGION: &str = "us-west-2";

fn p(key: &str, value: &str) -> (String, String) {
    (key.to_string(), value.to_string())
}

async fn ok(router: &MultiServiceRouter, action: &str, params: &[(String, String)]) -> String {
    let (status, body) = call(router, REGION, action, params).await;
    assert_eq!(status, 200, "{action} must succeed, got {status}: {body}");
    body
}

async fn default_acl_of(router: &MultiServiceRouter, vpc_id: &str) -> String {
    let body = ok(
        router,
        "DescribeNetworkAcls",
        &[
            p("Filter.1.Name", "vpc-id"),
            p("Filter.1.Value.1", vpc_id),
            p("Filter.2.Name", "default"),
            p("Filter.2.Value.1", "true"),
        ],
    )
    .await;
    element(&body, "networkAclId").unwrap_or_else(|| panic!("no default ACL for {vpc_id} in {body}"))
}

async fn tags_of(router: &MultiServiceRouter, acl_id: &str) -> String {
    ok(router, "DescribeNetworkAcls", &[p("NetworkAclId.1", acl_id)]).await
}

#[tokio::test]
async fn both_kinds_of_network_acl_accept_create_and_delete_tags() {
    let router = super::ec2_query::emulator().0;
    let vpc_body = ok(&router, "CreateVpc", &[p("CidrBlock", "10.40.0.0/16")]).await;
    let vpc_id = element(&vpc_body, "vpcId").expect("CreateVpc answers a vpcId");

    let default_acl = default_acl_of(&router, &vpc_id).await;
    let created_body = ok(&router, "CreateNetworkAcl", &[p("VpcId", &vpc_id)]).await;
    let created_acl = element(&created_body, "networkAclId").expect("CreateNetworkAcl answers an id");

    for (kind, acl_id) in [("default", &default_acl), ("created", &created_acl)] {
        ok(
            &router,
            "CreateTags",
            &[p("ResourceId.1", acl_id), p("Tag.1.Key", "Name"), p("Tag.1.Value", "acceptance-default")],
        )
        .await;
        let described = tags_of(&router, acl_id).await;
        assert!(
            described.contains("<key>Name</key>") && described.contains("<value>acceptance-default</value>"),
            "{kind} ACL {acl_id}: the tag must read back through DescribeNetworkAcls, got {described}"
        );

        ok(&router, "DeleteTags", &[p("ResourceId.1", acl_id), p("Tag.1.Key", "Name")]).await;
        let described = tags_of(&router, acl_id).await;
        assert!(
            !described.contains("<key>Name</key>"),
            "{kind} ACL {acl_id}: DeleteTags must remove the tag, got {described}"
        );
    }
}

#[tokio::test]
async fn an_unknown_network_acl_id_is_still_refused() {
    let router = super::ec2_query::emulator().0;
    let (status, body) = call(
        &router,
        REGION,
        "CreateTags",
        &[p("ResourceId.1", "acl-0123456789abcdef0"), p("Tag.1.Key", "Name"), p("Tag.1.Value", "x")],
    )
    .await;
    assert_eq!(status, 400, "an unknown ACL id must be refused, got {status}: {body}");
    assert_eq!(error_code(&body), "InvalidNetworkAclID.NotFound", "{body}");
}
