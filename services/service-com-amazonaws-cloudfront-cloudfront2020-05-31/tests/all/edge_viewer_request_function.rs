// SPDX-License-Identifier: CC0-1.0

//! fork: the VIEWER-REQUEST function a cache behaviour associates runs at the edge.
//!
//! Every case creates a function, publishes it, associates it with the default cache
//! behaviour of a deployed distribution and sends a viewer request through the router, so
//! what is asserted is what a viewer sees:
//!
//! * a RESPONSE the function returns is served as it stands, under `X-Cache:
//!   FunctionGeneratedResponse from cloudfront`, and the origin is never asked;
//! * a function that RAISES is a function execution error, answered `503` by CloudFront;
//! * the REQUEST the function returns is what the origin is asked for: a rewritten uri
//!   fetches the object it names;
//! * `cf.updateRequestOrigin` sends the request to the domain the function names, read here
//!   out of the associated key value store, as the LoopZero app router does.
//!
//! The origin that answers is this service's own control plane (`GET
//! /2020-05-31/distribution` answers a `DistributionList`), as in `edge_data_plane.rs`.

use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use service_com_amazonaws_cloudfront_cloudfront2020_05_31::EmulatedService;
use service_com_amazonaws_cloudfrontkeyvaluestore_cloudfrontkeyvaluestore::state as kvs;
use smithy_server_runtime::MultiServiceRouter;

use super::distribution_lifecycle::{
    call, create_distribution, past_propagation, API_VERSION, HOST,
};
use super::function_lifecycle::{create_key_value_store, Create};
use super::function_publish::{create_one, publish_function};

/// An origin NOTHING in this emulator serves (a Lambda function URL host), so a request that
/// reaches the origin the distribution configures answers `502`.
const UNSERVED_ORIGIN: &str = "a0b1c2d3e4f5g6h7.lambda-url.us-east-1.on.aws";

/// The viewer path the control plane answers with a `DistributionList`.
const SERVED_PATH: &str = "/2020-05-31/distribution";

/// One emulator carrying this service, and the engine it shares with the key value store
/// data plane, whose keys a case writes directly.
pub(crate) fn edge_emulator() -> (MultiServiceRouter, Arc<engine::InMemoryEngine>) {
    let engine: Arc<engine::InMemoryEngine> = Arc::new(engine::InMemoryEngine::new());
    let mut router = MultiServiceRouter::new();
    router.set_request_authentication(false);
    router.register_service(EmulatedService::new(Arc::clone(&engine)));
    (router, engine)
}

/// Create `code` as a `cloudfront-js-2.0` function associated with `stores`, publish it and
/// answer its ARN.
async fn published_function(router: &MultiServiceRouter, name: &str, code: &str, stores: Vec<String>) -> String {
    let create = Create {
        case: "a viewer-request function",
        name: name.to_string(),
        comment: "edge".to_string(),
        runtime: "cloudfront-js-2.0",
        code: code.as_bytes().to_vec(),
        associations: stores,
        tags: Vec::new(),
    };
    let etag = create_one(router, &create).await;
    let published = publish_function(router, name, &etag).await;
    assert_eq!(published.status, 200, "PublishFunction: {}", published.raw);
    format!("arn:aws:cloudfront::000000000000:function/{name}")
}

/// Create a distribution in front of `origin` whose default behaviour runs `function_arn`
/// on the viewer request, let it deploy, and answer its id and domain name.
async fn deployed_with(router: &MultiServiceRouter, reference: &str, origin: &str, function_arn: &str) -> (String, String) {
    let config = format!(
        "<DistributionConfig xmlns=\"http://cloudfront.amazonaws.com/doc/{API_VERSION}/\">\
         <CallerReference>{reference}</CallerReference>\
         <Origins><Quantity>1</Quantity><Items><Origin>\
         <Id>primary</Id><DomainName>{origin}</DomainName>\
         <CustomOriginConfig><HTTPPort>80</HTTPPort><HTTPSPort>443</HTTPSPort>\
         <OriginProtocolPolicy>https-only</OriginProtocolPolicy></CustomOriginConfig>\
         </Origin></Items></Origins>\
         <DefaultCacheBehavior><TargetOriginId>primary</TargetOriginId>\
         <ViewerProtocolPolicy>allow-all</ViewerProtocolPolicy>\
         <CachePolicyId>4135ea2d-6df8-44a3-9df3-4b5a84be39ad</CachePolicyId>\
         <FunctionAssociations><Quantity>1</Quantity><Items><FunctionAssociation>\
         <FunctionARN>{function_arn}</FunctionARN><EventType>viewer-request</EventType>\
         </FunctionAssociation></Items></FunctionAssociations>\
         </DefaultCacheBehavior>\
         <Comment>viewer request function</Comment>\
         <PriceClass>PriceClass_All</PriceClass>\
         <Enabled>true</Enabled>\
         </DistributionConfig>"
    );
    let created = create_distribution(router, &config).await;
    assert_eq!(created.status, 201, "CreateDistribution: {}", created.raw);
    let id = created.root().find_text("Id").expect("an id").to_string();
    let domain = created.root().find_text("DomainName").expect("a domain name").to_string();
    ::tokio::time::sleep(past_propagation()).await;
    let read = call(router, "GET", &format!("/{API_VERSION}/distribution/{id}"), None, None).await;
    assert_eq!(read.root().find_text("Status"), Some("Deployed"), "{}", read.raw);
    (id, domain)
}

struct Viewed {
    status: u16,
    x_cache: String,
    body: String,
}

/// One viewer request with `host` as its `Host`, to `path`.
async fn view(router: &MultiServiceRouter, host: &str, path: &str) -> Viewed {
    let request = ::http::Request::builder()
        .method("GET")
        .uri(path)
        .header("host", host)
        .body(Bytes::new())
        .expect("a well-formed request");
    let response = router.dispatch(request).await;
    let status = response.status().as_u16();
    let x_cache = response
        .headers()
        .get("x-cache")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = String::from_utf8_lossy(&response.into_body()).to_string();
    Viewed { status, x_cache, body }
}

/// A response the function returns is the viewer's answer, and the origin is never asked.
#[tokio::test]
async fn a_response_the_function_returns_is_served_without_the_origin() {
    let (router, _) = edge_emulator();
    let code = "function handler(event) {\n\
                  return { statusCode: 404, statusDescription: 'Not Found', \
                           headers: { 'content-type': { value: 'text/plain' } }, body: 'no route' };\n\
                }\n";
    let function = published_function(&router, "answers-404", code, Vec::new()).await;
    let (_, domain) = deployed_with(&router, "fn-response", HOST, &function).await;

    let viewed = view(&router, &domain, SERVED_PATH).await;
    assert_eq!(
        (viewed.status, viewed.x_cache.as_str(), viewed.body.as_str()),
        (404, "FunctionGeneratedResponse from cloudfront", "no route"),
        "the function's own response, with nothing of the origin's DistributionList"
    );
}

/// A function that raises is a function execution error: CloudFront answers 503 and the
/// origin is never asked.
#[tokio::test]
async fn a_function_that_throws_is_answered_with_a_503() {
    let (router, _) = edge_emulator();
    let code = "function handler(event) { throw new Error('boom'); }\n";
    let function = published_function(&router, "throws", code, Vec::new()).await;
    let (_, domain) = deployed_with(&router, "fn-throws", HOST, &function).await;

    let viewed = view(&router, &domain, SERVED_PATH).await;
    assert_eq!(viewed.status, 503, "{}", viewed.body);
    assert_eq!(viewed.x_cache, "Error from cloudfront");
    assert!(!viewed.body.contains("DistributionList"), "the origin answered: {}", viewed.body);
}

/// The request the function returns is what the origin is asked for: a viewer path the
/// origin does not serve, rewritten by the function, fetches the object the new uri names.
#[tokio::test]
async fn the_request_the_function_returns_is_what_the_origin_is_asked_for() {
    let (router, _) = edge_emulator();
    let code = "function handler(event) {\n\
                  var request = event.request;\n\
                  request.uri = '/2020-05-31/distribution';\n\
                  return request;\n\
                }\n";
    let function = published_function(&router, "rewrites", code, Vec::new()).await;
    let (_, domain) = deployed_with(&router, "fn-rewrite", HOST, &function).await;

    let viewed = view(&router, &domain, "/some/other/path").await;
    assert_eq!(viewed.status, 200, "{}", viewed.body);
    assert!(viewed.body.contains("DistributionList"), "{}", viewed.body);
}

/// The LoopZero app router: the function reads the viewer's host, looks its route and the
/// cell's origin up in the associated key value store, and sends the request there with
/// `cf.updateRequestOrigin`. The distribution's own origin is one nothing serves, so a 200
/// is the function's origin answering; a host with no route is the function's own 404.
#[tokio::test]
async fn update_request_origin_sends_the_request_to_the_origin_the_store_names() {
    let (router, engine) = edge_emulator();
    let store_arn = create_key_value_store(&router, "routes").await;
    {
        let state = kvs::service_state(&*engine).expect("the key value store state");
        let mut held = state.lock();
        let store = held.store_mut(&store_arn, SystemTime::now()).expect("the store");
        store.keys.insert("route:x".to_string(), "{\"cellId\":\"cell-0\"}".to_string());
        store.keys.insert("origin:cell-0".to_string(), HOST.to_string());
    }
    let code = "import cf from 'cloudfront';\n\
                var kvs = cf.kvs();\n\
                async function lookup(key) {\n\
                  try { return await kvs.get(key); } catch (e) { return null; }\n\
                }\n\
                async function handler(event) {\n\
                  var request = event.request;\n\
                  var host = request.headers.host.value;\n\
                  var label = host.split('.')[0];\n\
                  var route = await lookup('route:' + label);\n\
                  if (route === null) { return { statusCode: 404, statusDescription: 'Not Found' }; }\n\
                  var origin = await lookup('origin:' + JSON.parse(route).cellId);\n\
                  cf.updateRequestOrigin({ domainName: origin, hostHeader: label + '.example.com' });\n\
                  return request;\n\
                }\n";
    let function = published_function(&router, "app-router", code, vec![store_arn]).await;
    let (id, _) = deployed_with(&router, "fn-origin", UNSERVED_ORIGIN, &function).await;
    let prefixed = format!("/_aws/cloudfront/{id}{SERVED_PATH}");

    let routed = view(&router, "x.apps.example.com", &prefixed).await;
    assert_eq!(routed.status, 200, "{}", routed.body);
    assert!(routed.body.contains("DistributionList"), "{}", routed.body);

    let unknown = view(&router, "nobody.apps.example.com", &prefixed).await;
    assert_eq!(
        (unknown.status, unknown.x_cache.as_str()),
        (404, "FunctionGeneratedResponse from cloudfront"),
        "{}",
        unknown.body
    );
}
