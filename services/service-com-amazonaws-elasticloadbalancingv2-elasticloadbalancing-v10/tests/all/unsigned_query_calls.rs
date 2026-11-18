// SPDX-License-Identifier: CC0-1.0

//! fork: an unsigned query-protocol API call is not a load balancer's traffic.
//!
//! Some AWS API calls are sent with no signature at all: STS `AssumeRoleWithWebIdentity`
//! and `AssumeRoleWithSAML` are how a workload without credentials gets some, and an AWS
//! SDK sends them as an awsQuery form `POST /` carrying neither `Authorization` nor
//! `X-Amz-Date`. When the store held one Application Load Balancer with a rule for `/`
//! (the AWS Load Balancer Controller writes one for an Ingress path `/`), the data plane's
//! endpoint-only addressing claimed that call as well, STS claimed it too, and the router
//! answered `500 AmbiguousService`. A form `POST /` on the emulator's own endpoint is how
//! every query-protocol service is called, so it is left to them.

use std::time::{Duration, SystemTime};

use crate::alb_data_plane::{create_load_balancer, send, settle};
use crate::provisioning_loop::{action, call, emulator, p};

const EMULATOR_HOST: &str = "127.0.0.1:9297";
const ROOT_BODY: &str = "served by the root rule";

#[tokio::test]
async fn an_unsigned_form_post_to_the_root_is_left_to_the_query_services() {
    let (router, engine) = emulator();
    let (lb_arn, _dns_name) = create_load_balancer(&router, "unsigned-calls").await;

    let mut listener = action("CreateListener");
    listener.push(p("LoadBalancerArn", &lb_arn));
    listener.push(p("Protocol", "HTTP"));
    listener.push(p("Port", "80"));
    listener.push(p("DefaultActions.member.1.Type", "fixed-response"));
    listener.push(p("DefaultActions.member.1.FixedResponseConfig.StatusCode", "404"));
    let created = call(&router, &listener).await;
    assert_eq!(created.status, 200, "CreateListener: {}", created.body);
    let listener_arn = created.text("ListenerArn").expect("a listener ARN");

    let mut rule = action("CreateRule");
    rule.push(p("ListenerArn", &listener_arn));
    rule.push(p("Priority", "1"));
    rule.push(p("Conditions.member.1.Field", "path-pattern"));
    rule.push(p("Conditions.member.1.Values.member.1", "/*"));
    rule.push(p("Actions.member.1.Type", "fixed-response"));
    rule.push(p("Actions.member.1.FixedResponseConfig.StatusCode", "200"));
    rule.push(p("Actions.member.1.FixedResponseConfig.MessageBody", ROOT_BODY));
    let created = call(&router, &rule).await;
    assert_eq!(created.status, 200, "CreateRule: {}", created.body);
    settle(&engine, &lb_arn, SystemTime::now() - Duration::from_secs(1));

    // The rule is reachable by the path alone, so what follows is decided by the request
    // being an API call and not by the load balancer serving nothing.
    let browsed = send(&router, "GET", EMULATOR_HOST, "/", &[]).await;
    assert_eq!((browsed.status, browsed.body.as_str()), (200, ROOT_BODY));

    for content_type in ["application/x-www-form-urlencoded", "application/x-www-form-urlencoded; charset=utf-8"] {
        let request = http::Request::builder()
            .method("POST")
            .uri(format!("http://{EMULATOR_HOST}/"))
            .header("host", EMULATOR_HOST)
            .header("content-type", content_type)
            .body(bytes::Bytes::from_static(
                b"Action=AssumeRoleWithWebIdentity&Version=2011-06-15&RoleArn=arn%3Aaws%3Aiam%3A%3A000000000000%3Arole%2Fx&RoleSessionName=s&WebIdentityToken=token",
            ))
            .expect("a well-formed request");
        let response = router.dispatch(request).await;
        let body = String::from_utf8_lossy(response.body()).into_owned();
        assert_ne!(
            body, ROOT_BODY,
            "an unsigned query call ({content_type}) was answered as load balancer traffic"
        );
    }
}
