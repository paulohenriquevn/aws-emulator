// SPDX-License-Identifier: CC0-1.0

//! fork: a listener's default rule is a taggable resource like any rule a caller adds.
//!
//! `DescribeRules` reports every listener's default rule with its own ARN, and AWS lets
//! `DescribeTags`, `AddTags` and `RemoveTags` name it. The AWS Load Balancer Controller
//! does exactly that while it reconciles an Ingress: it reads the tags of every rule of
//! the listener it manages, the default rule included, and stops reconciling when that
//! read is refused.

use crate::listener_rules::{create_listener, create_load_balancer};
use crate::provisioning_loop::{
    action, call, create_http_target_group, emulator, first_text, members, p,
};
use crate::resource_tags::{describe_tags, reported};
use smithy_server_runtime::MultiServiceRouter;

/// The ARN `DescribeRules` reports for `listener_arn`'s default rule.
async fn default_rule_arn(router: &MultiServiceRouter, listener_arn: &str) -> String {
    let mut form = action("DescribeRules");
    form.push(p("ListenerArn", listener_arn));
    let reply = call(router, &form).await;
    assert_eq!(reply.status, 200, "DescribeRules: expected 200, got {} body {}", reply.status, reply.body);
    members(&reply.body, "Rules")
        .iter()
        .find(|rule| first_text(rule, "IsDefault").as_deref() == Some("true"))
        .and_then(|rule| first_text(rule, "RuleArn"))
        .unwrap_or_else(|| panic!("DescribeRules reported no default rule: {}", reply.body))
}

async fn listener_with_default_rule(router: &MultiServiceRouter) -> String {
    let lb = create_load_balancer(router, "default-rule-tags").await;
    let tg = create_http_target_group(router, "default-rule-tags").await;
    let listener = create_listener(router, &lb, &tg, 80).await;
    default_rule_arn(router, &listener).await
}

#[tokio::test]
async fn the_default_rule_of_a_listener_reports_its_tags() {
    let (router, _engine) = emulator();
    let rule = listener_with_default_rule(&router).await;

    let reply = describe_tags(&router, std::slice::from_ref(&rule)).await;

    assert_eq!(reply.status, 200, "DescribeTags of the default rule {rule}: got {} body {}", reply.status, reply.body);
    assert_eq!(reported(&reply), vec![(rule, Vec::new())]);
}

#[tokio::test]
async fn tags_written_to_the_default_rule_are_read_back_and_removed() {
    let (router, _engine) = emulator();
    let rule = listener_with_default_rule(&router).await;

    let mut add = action("AddTags");
    add.push(p("ResourceArns.member.1", &rule));
    add.push(p("Tags.member.1.Key", "elbv2.k8s.aws/cluster"));
    add.push(p("Tags.member.1.Value", "operations"));
    add.push(p("Tags.member.2.Key", "ingress.k8s.aws/stack"));
    add.push(p("Tags.member.2.Value", "probes/echo"));
    let added = call(&router, &add).await;
    assert_eq!(added.status, 200, "AddTags on the default rule {rule}: got {} body {}", added.status, added.body);

    let tagged = describe_tags(&router, std::slice::from_ref(&rule)).await;
    assert_eq!(
        reported(&tagged),
        vec![(
            rule.clone(),
            vec![
                ("elbv2.k8s.aws/cluster".to_string(), "operations".to_string()),
                ("ingress.k8s.aws/stack".to_string(), "probes/echo".to_string()),
            ]
        )]
    );

    let mut remove = action("RemoveTags");
    remove.push(p("ResourceArns.member.1", &rule));
    remove.push(p("TagKeys.member.1", "ingress.k8s.aws/stack"));
    let removed = call(&router, &remove).await;
    assert_eq!(removed.status, 200, "RemoveTags on the default rule {rule}: got {} body {}", removed.status, removed.body);

    let left = describe_tags(&router, std::slice::from_ref(&rule)).await;
    assert_eq!(
        reported(&left),
        vec![(rule, vec![("elbv2.k8s.aws/cluster".to_string(), "operations".to_string())])]
    );
}
