// SPDX-License-Identifier: CC0-1.0

//! A distribution's viewer-request CloudFront Function routes a tenant host to the cell that
//! serves it, end to end through the router the binary serves, with request authentication
//! and IAM enforcement on, as LoopZero's staging runs it:
//!
//! * a key value store holds `route:<label>` and `origin:<cell>`, written over its SigV4A
//!   data plane;
//! * a `cloudfront-js-2.0` function associated with that store reads both and calls
//!   `cf.updateRequestOrigin({domainName, hostHeader})`;
//! * the cell is an Application Load Balancer this emulator serves, forwarding to an `ip`
//!   target that is a real HTTP server on a loopback port, which answers with the request it
//!   received, so the `Host` asserted is the one the target saw.

mod support;

use std::time::{Duration, SystemTime};

use bytes::Bytes;
use service_com_amazonaws_ec2_amazonec2::state as ec2;
use service_com_amazonaws_elasticloadbalancingv2_elasticloadbalancing_v10::state::ComAmazonawsElasticloadbalancingv2ElasticloadbalancingV10State as ElbState;
use support::{
    create_key_value_store, element, on_authenticating_router, segment, send, signed_by_emulator, Reply, Router, Scheme,
    CLOUDFRONT, KEY_VALUE_STORE_DATA_PLANE,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The LoopZero app router's code (`infra-modules` `modules/edge/app-router/function.js`), with
/// the two suffixes Terraform fills in: tenant hosts are `<label>.apps.example.com`, and the
/// cell's ingress serves `<label>.example.com`.
const APP_ROUTER: &str = r#"// Viewer request function (cloudfront-js-2.0) in front of the cells.
import cf from 'cloudfront';

var kvs = cf.kvs();
var PUBLIC_SUFFIX = '.apps.example.com';
var INTERNAL_SUFFIX = '.example.com';

function answer(status, description) {
  return { statusCode: status, statusDescription: description };
}

async function lookup(key) {
  try {
    return await kvs.get(key);
  } catch (e) {
    return null;
  }
}

async function handler(event) {
  var request = event.request;
  var host = request.headers.host ? request.headers.host.value.toLowerCase() : '';
  var routeKey = host;
  var originHost = host;
  if (host.endsWith(PUBLIC_SUFFIX)) {
    var label = host.slice(0, host.length - PUBLIC_SUFFIX.length);
    if (label === '' || label.indexOf('.') >= 0) {
      return answer(404, 'Not Found');
    }
    routeKey = label;
    originHost = label + INTERNAL_SUFFIX;
  }
  if (routeKey === '') {
    return answer(404, 'Not Found');
  }

  var route = await lookup('route:' + routeKey);
  var cellId = null;
  try {
    cellId = route === null ? null : JSON.parse(route).cellId;
  } catch (e) {
    cellId = null;
  }
  if (!cellId) {
    return answer(404, 'Not Found');
  }
  var origin = await lookup('origin:' + cellId);
  if (!origin) {
    return answer(503, 'Service Unavailable');
  }
  cf.updateRequestOrigin({ domainName: origin, hostHeader: originHost });
  return request;
}
"#;

/// A function whose handler raises.
const THROWS: &str = "function handler(event) { throw new Error('the router broke'); }\n";

/// An HTTP/1.1 server on a loopback port that answers every request with the request head
/// it read as its body. Answers its port.
async fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a loopback port");
    let port = listener.local_addr().expect("a bound address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut received = Vec::new();
                let mut buffer = [0u8; 4096];
                while !received.windows(4).any(|window| window == b"\r\n\r\n") {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => received.extend_from_slice(&buffer[..read]),
                    }
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    received.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(&received).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    port
}

const ELB: Scheme = Scheme::SigV4 { signing_name: "elasticloadbalancing", region: "us-east-1" };

/// One awsQuery call to Elastic Load Balancing v2.
async fn elb(router: &Router, fields: &[(&str, &str)]) -> Reply {
    let mut all = vec![("Version", "2015-12-01")];
    all.extend_from_slice(fields);
    let body = all
        .iter()
        .map(|(name, value)| format!("{}={}", segment(name), segment(value)))
        .collect::<Vec<_>>()
        .join("&");
    let headers = [("content-type", "application/x-www-form-urlencoded")];
    let reply = send(router, signed_by_emulator(&ELB, "POST", "/", &headers, &body)).await;
    assert_eq!(reply.status, 200, "{:?}: {}", fields.first(), reply.text);
    reply
}

/// The address the cell's target is registered at, in the cell's first subnet.
const CELL_TARGET: &str = "10.40.1.10";

const EC2: Scheme = Scheme::SigV4 { signing_name: "ec2", region: "us-east-1" };

/// One EC2 Query call; answers the text of the first `<answer>` element of its response.
async fn ec2(router: &Router, fields: &[(&str, &str)], answer: &str) -> String {
    let mut all = vec![("Version", "2016-11-15")];
    all.extend_from_slice(fields);
    let body = all
        .iter()
        .map(|(name, value)| format!("{}={}", segment(name), segment(value)))
        .collect::<Vec<_>>()
        .join("&");
    let headers = [("content-type", "application/x-www-form-urlencoded")];
    let reply = send(router, signed_by_emulator(&EC2, "POST", "/", &headers, &body)).await;
    assert_eq!(reply.status, 200, "{:?}: {}", fields.first(), reply.text);
    element(&reply.text, answer)
}

/// A VPC with a subnet in each of two Availability Zones, as an Application Load Balancer
/// needs; answers the VPC and the two subnets.
async fn cell_network(router: &Router) -> (String, String, String) {
    let vpc = ec2(router, &[("Action", "CreateVpc"), ("CidrBlock", "10.40.0.0/16")], "vpcId").await;
    let mut subnets = Vec::new();
    for (zone, cidr) in [("us-east-1a", "10.40.1.0/24"), ("us-east-1b", "10.40.2.0/24")] {
        let fields = [("Action", "CreateSubnet"), ("VpcId", vpc.as_str()), ("CidrBlock", cidr), ("AvailabilityZone", zone)];
        subnets.push(ec2(router, &fields, "subnetId").await);
    }
    let second = subnets.pop().expect("two subnets");
    let first = subnets.pop().expect("two subnets");
    (vpc, first, second)
}

/// An active Application Load Balancer whose HTTP:80 listener forwards to `127.0.0.1:<port>`;
/// answers its DNS name.
async fn cell_load_balancer(router: &Router, engine: &engine::InMemoryEngine, port: u16) -> String {
    let (vpc, first, second) = cell_network(router).await;
    let created = elb(
        router,
        &[
            ("Action", "CreateLoadBalancer"),
            ("Name", "cell-0"),
            ("Subnets.member.1", &first),
            ("Subnets.member.2", &second),
        ],
    )
    .await;
    let (arn, dns_name) = (element(&created.text, "LoadBalancerArn"), element(&created.text, "DNSName"));
    let port = port.to_string();
    let group = elb(
        router,
        &[
            ("Action", "CreateTargetGroup"),
            ("Name", "cell-0-ingress"),
            ("Protocol", "HTTP"),
            ("Port", &port),
            ("VpcId", &vpc),
            ("TargetType", "ip"),
        ],
    )
    .await;
    let group_arn = element(&group.text, "TargetGroupArn");
    elb(
        router,
        &[("Action", "RegisterTargets"), ("TargetGroupArn", &group_arn), ("Targets.member.1.Id", CELL_TARGET)],
    )
    .await;
    elb(
        router,
        &[
            ("Action", "CreateListener"),
            ("LoadBalancerArn", &arn),
            ("Protocol", "HTTP"),
            ("Port", "80"),
            ("DefaultActions.member.1.Type", "forward"),
            ("DefaultActions.member.1.TargetGroupArn", &group_arn),
        ],
    )
    .await;
    // The provisioning and the first health checks are owed by the clock; they are settled
    // here rather than waited for. The target is registered at an address of the cell's VPC,
    // as an `ip` target must be, and moved to the loopback address the echo server listens
    // on, which is the one address this test can serve. A health check finds an `ip` target
    // healthy when an interface of the VPC holds its address, so one is put there, as the
    // load balancer's own forwarding tests do.
    let ec2_state = ec2::service_state(engine).expect("the shared engine holds an EC2 inventory");
    ec2_state
        .lock()
        .insert_network_interface(ec2::NetworkInterfaceSpec {
            subnet_id: first,
            vpc_id: vpc,
            availability_zone: "us-east-1a".to_string(),
            region: "us-east-1".to_string(),
            description: "the cell's ingress".to_string(),
            interface_type: ec2::NETWORK_INTERFACE_TYPE_INTERFACE.to_string(),
            private_ip_addresses: vec!["127.0.0.1".to_string()],
            ..ec2::NetworkInterfaceSpec::default()
        })
        .expect("the inventory holds the interface");
    let state = engine.state::<ElbState>();
    let mut store = state.lock();
    let settled = SystemTime::now() - Duration::from_secs(1);
    if let Some(record) = store.load_balancers.get_mut(&arn) {
        record.active_at = settled;
    }
    for group in store.target_groups.values_mut() {
        for target in group.targets.values_mut() {
            target.healthy_at = settled;
            if target.id == CELL_TARGET {
                target.id = "127.0.0.1".to_string();
            }
        }
    }
    dns_name
}

/// Put `key` = `value` into the store over its data plane.
async fn put_key(router: &Router, store_arn: &str, key: &str, value: &str) {
    let store = format!("/key-value-stores/{}", segment(store_arn));
    let headers = [("content-type", "application/json")];
    let described = send(router, signed_by_emulator(&KEY_VALUE_STORE_DATA_PLANE, "GET", &store, &headers, "")).await;
    assert_eq!(described.status, 200, "DescribeKeyValueStore: {}", described.text);
    let etag = described.etag();
    let path = format!("{store}/keys/{}", segment(key));
    let body = serde_json::json!({ "Value": value }).to_string();
    let headers = [("content-type", "application/json"), ("if-match", etag.as_str())];
    let put = send(router, signed_by_emulator(&KEY_VALUE_STORE_DATA_PLANE, "PUT", &path, &headers, &body)).await;
    assert_eq!(put.status, 200, "PutKey {key}: {}", put.text);
}

/// Create `code` as a `cloudfront-js-2.0` function associated with `store_arn` (when given),
/// publish it, and answer its ARN.
async fn published_function(router: &Router, name: &str, code: &str, store_arn: Option<&str>) -> String {
    let associations = store_arn
        .map(|arn| {
            format!(
                "<KeyValueStoreAssociations><Quantity>1</Quantity><Items><KeyValueStoreAssociation>\
                 <KeyValueStoreARN>{arn}</KeyValueStoreARN></KeyValueStoreAssociation></Items>\
                 </KeyValueStoreAssociations>"
            )
        })
        .unwrap_or_default();
    let code = aws_smithy_types::base64::encode(code);
    let body = format!(
        "<CreateFunctionRequest xmlns=\"http://cloudfront.amazonaws.com/doc/2020-05-31/\">\
         <Name>{name}</Name><FunctionConfig><Comment>app router</Comment>\
         <Runtime>cloudfront-js-2.0</Runtime>{associations}</FunctionConfig>\
         <FunctionCode>{code}</FunctionCode></CreateFunctionRequest>"
    );
    let headers = [("content-type", "application/xml")];
    let created = send(router, signed_by_emulator(&CLOUDFRONT, "POST", "/2020-05-31/function", &headers, &body)).await;
    assert_eq!(created.status, 201, "CreateFunction: {}", created.text);
    let etag = created.etag();
    let path = format!("/2020-05-31/function/{name}/publish");
    let published = send(router, signed_by_emulator(&CLOUDFRONT, "POST", &path, &[("if-match", &etag)], "")).await;
    assert_eq!(published.status, 200, "PublishFunction: {}", published.text);
    element(&published.text, "FunctionARN")
}

/// A deployed distribution whose default behaviour runs `function_arn` on the viewer request;
/// answers its id. The behaviour caches nothing (`CachingDisabled`) and forwards every
/// viewer header but `Host` and the query string (`AllViewerExceptHostHeader`), as an app
/// router's does: without an origin request policy CloudFront forwards no query string.
async fn distribution(router: &Router, reference: &str, function_arn: &str) -> String {
    let body = format!(
        "<DistributionConfig xmlns=\"http://cloudfront.amazonaws.com/doc/2020-05-31/\">\
         <CallerReference>{reference}</CallerReference>\
         <Origins><Quantity>1</Quantity><Items><Origin>\
         <Id>cells</Id><DomainName>cells.example.com</DomainName>\
         <CustomOriginConfig><HTTPPort>80</HTTPPort><HTTPSPort>443</HTTPSPort>\
         <OriginProtocolPolicy>http-only</OriginProtocolPolicy></CustomOriginConfig>\
         </Origin></Items></Origins>\
         <DefaultCacheBehavior><TargetOriginId>cells</TargetOriginId>\
         <ViewerProtocolPolicy>allow-all</ViewerProtocolPolicy>\
         <CachePolicyId>4135ea2d-6df8-44a3-9df3-4b5a84be39ad</CachePolicyId>\
         <OriginRequestPolicyId>b689b0a8-53d0-40ab-baf2-68738e2966ac</OriginRequestPolicyId>\
         <FunctionAssociations><Quantity>1</Quantity><Items><FunctionAssociation>\
         <FunctionARN>{function_arn}</FunctionARN><EventType>viewer-request</EventType>\
         </FunctionAssociation></Items></FunctionAssociations>\
         </DefaultCacheBehavior>\
         <Comment>tenant apps</Comment><PriceClass>PriceClass_All</PriceClass><Enabled>true</Enabled>\
         </DistributionConfig>"
    );
    let headers = [("content-type", "application/xml")];
    let created = send(router, signed_by_emulator(&CLOUDFRONT, "POST", "/2020-05-31/distribution", &headers, &body)).await;
    assert_eq!(created.status, 201, "CreateDistribution: {}", created.text);
    let id = element(&created.text, "Id");
    let path = format!("/2020-05-31/distribution/{id}");
    for _ in 0..100 {
        let read = send(router, signed_by_emulator(&CLOUDFRONT, "GET", &path, &[], "")).await;
        if element(&read.text, "Status") == "Deployed" {
            return id;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("the distribution {id} did not deploy");
}

/// One viewer request, unsigned, with `host` as its `Host`, addressed to the distribution's
/// data plane on the emulator endpoint.
async fn view(router: &Router, id: &str, host: &str, path: &str) -> Reply {
    let request = http::Request::builder()
        .method("GET")
        .uri(format!("/_aws/cloudfront/{id}{path}"))
        .header("host", host)
        .body(Bytes::new())
        .expect("a well-formed request");
    send(router, request).await
}

#[test]
fn the_function_sends_a_tenant_host_to_its_cell_with_the_internal_host() {
    on_authenticating_router(|router, engine| {
        Box::pin(async move {
            let target = echo_server().await;
            let cell = cell_load_balancer(&router, &engine, target).await;
            let store = create_key_value_store(&router, "app-routes").await;
            put_key(&router, &store, "route:x", r#"{"cellId":"cell-0"}"#).await;
            put_key(&router, &store, "origin:cell-0", &cell).await;
            let function = published_function(&router, "app-router", APP_ROUTER, Some(&store)).await;
            let id = distribution(&router, "tenant-apps", &function).await;

            let routed = view(&router, &id, "x.apps.example.com", "/dashboard?tab=1").await;
            assert_eq!(routed.status, 200, "x-cache {:?}: {}", routed.header("x-cache"), routed.text);
            let seen = routed.text.to_ascii_lowercase();
            assert!(seen.starts_with("get /dashboard?tab=1 "), "the target saw {seen}");
            assert!(seen.contains("\r\nhost: x.example.com\r\n"), "the target saw {seen}");
        })
    });
}

#[test]
fn a_host_with_no_route_is_answered_404_by_the_function() {
    on_authenticating_router(|router, _engine| {
        Box::pin(async move {
            let store = create_key_value_store(&router, "empty-routes").await;
            let function = published_function(&router, "app-router-empty", APP_ROUTER, Some(&store)).await;
            let id = distribution(&router, "no-routes", &function).await;

            let unknown = view(&router, &id, "nobody.apps.example.com", "/").await;
            assert_eq!(unknown.status, 404, "{}", unknown.text);
            assert_eq!(unknown.header("x-cache"), "FunctionGeneratedResponse from cloudfront");
        })
    });
}

#[test]
fn a_function_that_throws_is_answered_503() {
    on_authenticating_router(|router, _engine| {
        Box::pin(async move {
            let function = published_function(&router, "broken-router", THROWS, None).await;
            let id = distribution(&router, "broken", &function).await;

            let failed = view(&router, &id, "x.apps.example.com", "/").await;
            assert_eq!(failed.status, 503, "{}", failed.text);
            assert_eq!(failed.header("x-cache"), "Error from cloudfront");
        })
    });
}
