// SPDX-License-Identifier: CC0-1.0

//! A managed node group runs EC2 instances: `CreateNodegroup` with `desiredSize` 2 leaves two
//! running instances tagged with the node group and the cluster, one in each of the node
//! group's subnets, and `DeleteNodegroup` terminates them
//! (services/service-com-amazonaws-eks-awswesleyfrontend/src/state/nodegroup_instances.rs).

use aws_emulator_fork_router::{build_services, InMemoryEngine};
use bytes::Bytes;
use serde_json::{json, Value};
use smithy_server_runtime::MultiServiceRouter;
use std::sync::Arc;
use std::time::{Duration, Instant};

const REGION: &str = "us-west-2";
const DEADLINE: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Assembling the router needs a larger stack than a test thread has (see main.rs).
const ROUTER_STACK_SIZE: usize = 64 * 1024 * 1024;

fn on_router_thread(case: impl FnOnce(MultiServiceRouter) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(ROUTER_STACK_SIZE)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(ROUTER_STACK_SIZE)
                .enable_all()
                .build()
                .expect("a tokio runtime");
            let mut router = build_services(Arc::new(InMemoryEngine::new()));
            router.set_request_authentication(false);
            runtime.block_on(case(router));
        })
        .expect("spawn the router thread")
        .join()
        .expect("the case passes");
}

fn authorization(service: &str) -> String {
    format!(
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260101/{REGION}/{service}/aws4_request, \
         SignedHeaders=host, Signature=0000"
    )
}

/// One form-encoded query call to `service` (EC2 or IAM); answers the XML body.
async fn query(router: &MultiServiceRouter, service: &str, host: &str, action: &str, version: &str, params: &[(&str, &str)]) -> String {
    let mut body = format!("Action={action}&Version={version}");
    for (key, value) in params {
        body.push_str(&format!("&{key}={}", urlencode(value)));
    }
    let request = http::Request::builder()
        .method("POST")
        .uri(format!("https://{host}/"))
        .header("host", host)
        .header("authorization", authorization(service))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Bytes::from(body))
        .expect("a well-formed request");
    let response = router.dispatch(request).await;
    let status = response.status().as_u16();
    let text = String::from_utf8_lossy(response.body()).into_owned();
    assert_eq!(status, 200, "{action} answered {status}: {text}");
    text
}

async fn ec2(router: &MultiServiceRouter, action: &str, params: &[(&str, &str)]) -> String {
    query(router, "ec2", &format!("ec2.{REGION}.amazonaws.com"), action, "2016-11-15", params).await
}

/// One REST JSON call to EKS; answers the status and the parsed body.
async fn eks(router: &MultiServiceRouter, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let request = http::Request::builder()
        .method(method)
        .uri(format!("https://eks.{REGION}.amazonaws.com{path}"))
        .header("host", format!("eks.{REGION}.amazonaws.com"))
        .header("authorization", authorization("eks"))
        .header("content-type", "application/json")
        .body(Bytes::from(body.map(|body| body.to_string()).unwrap_or_default()))
        .expect("a well-formed request");
    let response = router.dispatch(request).await;
    let status = response.status().as_u16();
    (status, serde_json::from_slice(response.body()).unwrap_or(Value::Null))
}

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Every value of the XML elements named `name`.
fn elements(xml: &str, name: &str) -> Vec<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    xml.split(&open)
        .skip(1)
        .filter_map(|rest| rest.split_once(&close).map(|(value, _)| value.to_string()))
        .collect()
}

/// The node group's instances EC2 holds in one of `states`, as (instance id, subnet).
async fn nodes_in(router: &MultiServiceRouter, nodegroup: &str, states: &str) -> Vec<(String, String)> {
    let described = ec2(
        router,
        "DescribeInstances",
        &[
            ("Filter.1.Name", "tag:eks:nodegroup-name"),
            ("Filter.1.Value.1", nodegroup),
            ("Filter.2.Name", "tag:eks:cluster-name"),
            ("Filter.2.Value.1", "nodes"),
            ("Filter.3.Name", "instance-state-name"),
            ("Filter.3.Value.1", states),
        ],
    )
    .await;
    // Each instance's own `subnetId` is the first one after its `instanceId` (its network
    // interfaces repeat it further on).
    described
        .split("<instanceId>")
        .skip(1)
        .filter_map(|rest| {
            let (id, after) = rest.split_once("</instanceId>")?;
            let subnet = elements(after, "subnetId").into_iter().next()?;
            Some((id.to_string(), subnet))
        })
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_iter()
        .collect()
}

async fn wait_for(router: &MultiServiceRouter, nodegroup: &str, states: &str, count: usize) -> Vec<(String, String)> {
    let started = Instant::now();
    loop {
        let nodes = nodes_in(router, nodegroup, states).await;
        if nodes.len() == count {
            return nodes;
        }
        assert!(started.elapsed() < DEADLINE, "{count} {states} nodes of {nodegroup} were wanted, EC2 holds {nodes:?}");
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// An ACTIVE cluster `nodes` in a VPC with two subnets; answers the subnets and a node role.
async fn cluster(router: &MultiServiceRouter) -> (Vec<String>, String) {
    let vpc = elements(&ec2(router, "CreateVpc", &[("CidrBlock", "10.70.0.0/16")]).await, "vpcId").remove(0);
    let mut subnets = Vec::new();
    for (cidr, zone) in [("10.70.1.0/24", "us-west-2a"), ("10.70.2.0/24", "us-west-2b")] {
        let created = ec2(router, "CreateSubnet", &[("VpcId", &vpc), ("CidrBlock", cidr), ("AvailabilityZone", zone)]).await;
        subnets.push(elements(&created, "subnetId").remove(0));
    }
    let trust = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":["eks.amazonaws.com","ec2.amazonaws.com"]},"Action":"sts:AssumeRole"}]}"#;
    let role = query(router, "iam", "iam.amazonaws.com", "CreateRole", "2010-05-08", &[("RoleName", "nodes"), ("AssumeRolePolicyDocument", trust)]).await;
    let role_arn = elements(&role, "Arn").remove(0);
    let (status, created) = eks(
        router,
        "POST",
        "/clusters",
        Some(json!({"name": "nodes", "version": "1.34", "roleArn": role_arn, "resourcesVpcConfig": {"subnetIds": subnets}})),
    )
    .await;
    assert_eq!(status, 200, "CreateCluster: {created}");
    let started = Instant::now();
    loop {
        let (_, described) = eks(router, "GET", "/clusters/nodes", None).await;
        if described["cluster"]["status"] == json!("ACTIVE") {
            break;
        }
        assert!(started.elapsed() < DEADLINE, "the cluster never became ACTIVE: {described}");
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    (subnets, role_arn)
}

async fn create_nodegroup(router: &MultiServiceRouter, name: &str, subnets: &[String], role_arn: &str) {
    let (status, created) = eks(
        router,
        "POST",
        "/clusters/nodes/node-groups",
        Some(json!({
            "nodegroupName": name,
            "subnets": subnets,
            "nodeRole": role_arn,
            "scalingConfig": {"minSize": 1, "maxSize": 3, "desiredSize": 2}
        })),
    )
    .await;
    assert_eq!(status, 200, "CreateNodegroup: {created}");
}

#[test]
fn a_node_group_runs_its_desired_size_of_instances_one_per_subnet() {
    on_router_thread(|router| {
        Box::pin(async move {
            let (subnets, role_arn) = cluster(&router).await;

            create_nodegroup(&router, "workers", &subnets, &role_arn).await;

            let nodes = wait_for(&router, "workers", "running", 2).await;
            let mut placed: Vec<String> = nodes.into_iter().map(|(_, subnet)| subnet).collect();
            placed.sort();
            let mut expected = subnets.clone();
            expected.sort();
            assert_eq!(placed, expected);
        })
    });
}

#[test]
fn a_deleted_node_group_terminates_its_instances() {
    on_router_thread(|router| {
        Box::pin(async move {
            let (subnets, role_arn) = cluster(&router).await;
            create_nodegroup(&router, "doomed", &subnets, &role_arn).await;
            wait_for(&router, "doomed", "running", 2).await;

            let (status, deleted) = eks(&router, "DELETE", "/clusters/nodes/node-groups/doomed", None).await;
            assert_eq!(status, 200, "DeleteNodegroup: {deleted}");

            wait_for(&router, "doomed", "terminated", 2).await;
        })
    });
}
