// SPDX-License-Identifier: CC0-1.0

//! A revoke of an all-protocols rule matches the stored rule whatever port range it names.
//!
//! "Specifying -1 or a protocol number other than tcp, udp, icmp, or icmpv6 allows traffic
//! on all ports, regardless of any port range you specify" (`IpPermission$IpProtocol`). So
//! the port range is no part of such a rule's identity. The Terraform AWS provider revokes
//! every new group's default egress rule as `IpProtocol -1, FromPort 0, ToPort 0`, and AWS
//! accepts it; a port-exact match here failed every `aws_security_group`.
//!
//! The groups below live in a NON-default VPC, where a rule the group does not hold is
//! refused with `InvalidPermission.NotFound` rather than forgiven, so a wrong match is
//! visible as an error.

use smithy_server_runtime::MultiServiceRouter;

use super::ec2_query::{call, element, error_code};

const REGION: &str = "us-west-2";

fn p(key: &str, value: &str) -> (String, String) {
    (key.to_string(), value.to_string())
}

async fn new_group(router: &MultiServiceRouter, cidr: &str) -> String {
    let (status, body) = call(router, REGION, "CreateVpc", &[p("CidrBlock", cidr)]).await;
    assert_eq!(status, 200, "CreateVpc: {body}");
    let vpc_id = element(&body, "vpcId").expect("vpcId");
    let (status, body) = call(
        router,
        REGION,
        "CreateSecurityGroup",
        &[p("GroupName", "g"), p("GroupDescription", "d"), p("VpcId", &vpc_id)],
    )
    .await;
    assert_eq!(status, 200, "CreateSecurityGroup: {body}");
    element(&body, "groupId").expect("groupId")
}

fn revoke_all_traffic_egress(group_id: &str, ports: Option<(&str, &str)>) -> Vec<(String, String)> {
    let mut params = vec![
        p("GroupId", group_id),
        p("IpPermissions.1.IpProtocol", "-1"),
        p("IpPermissions.1.IpRanges.1.CidrIp", "0.0.0.0/0"),
    ];
    if let Some((from, to)) = ports {
        params.push(p("IpPermissions.1.FromPort", from));
        params.push(p("IpPermissions.1.ToPort", to));
    }
    params
}

async fn egress_rules_of(router: &MultiServiceRouter, group_id: &str) -> String {
    let (status, body) = call(router, REGION, "DescribeSecurityGroups", &[p("GroupId.1", group_id)]).await;
    assert_eq!(status, 200, "DescribeSecurityGroups: {body}");
    body
}

#[tokio::test]
async fn default_egress_rule_is_revoked_whatever_port_range_the_request_names() {
    for (case, ports) in [
        ("no ports", None),
        ("0-0, as the Terraform AWS provider sends", Some(("0", "0"))),
        ("-1 to -1", Some(("-1", "-1"))),
        ("1-65535", Some(("1", "65535"))),
    ] {
        let router = super::ec2_query::emulator().0;
        let group_id = new_group(&router, "10.50.0.0/16").await;

        let (status, body) =
            call(&router, REGION, "RevokeSecurityGroupEgress", &revoke_all_traffic_egress(&group_id, ports)).await;

        assert_eq!(status, 200, "case {case:?}: the default egress rule must be revoked, got {status}: {body}");
        assert!(body.contains("<return>true</return>"), "case {case:?}: {body}");
        let described = egress_rules_of(&router, &group_id).await;
        assert!(
            !described.contains("<cidrIp>0.0.0.0/0</cidrIp>"),
            "case {case:?}: the group must hold no egress rule afterwards, got {described}"
        );
    }
}

#[tokio::test]
async fn a_tcp_rule_still_matches_its_ports_exactly() {
    let router = super::ec2_query::emulator().0;
    let group_id = new_group(&router, "10.51.0.0/16").await;
    let authorize = [
        p("GroupId", &group_id),
        p("IpPermissions.1.IpProtocol", "tcp"),
        p("IpPermissions.1.FromPort", "443"),
        p("IpPermissions.1.ToPort", "443"),
        p("IpPermissions.1.IpRanges.1.CidrIp", "10.0.0.0/8"),
    ];
    let (status, body) = call(&router, REGION, "AuthorizeSecurityGroupEgress", &authorize).await;
    assert_eq!(status, 200, "AuthorizeSecurityGroupEgress: {body}");

    let other_port = [
        p("GroupId", &group_id),
        p("IpPermissions.1.IpProtocol", "tcp"),
        p("IpPermissions.1.FromPort", "80"),
        p("IpPermissions.1.ToPort", "80"),
        p("IpPermissions.1.IpRanges.1.CidrIp", "10.0.0.0/8"),
    ];
    let (status, body) = call(&router, REGION, "RevokeSecurityGroupEgress", &other_port).await;

    assert_eq!(status, 400, "a tcp revoke naming other ports must not match, got {status}: {body}");
    assert_eq!(error_code(&body), "InvalidPermission.NotFound", "{body}");
}
