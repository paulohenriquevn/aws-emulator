//! The request body cap of the production serve path. `docker push` sends a whole image
//! layer in one request, so a deployment that receives layers through the ECR registry
//! endpoint raises the cap with EMULATOR_MAX_BODY_BYTES.

use emulator_server::{parse_body_limit, read_production_body, DEFAULT_MAX_BODY_BYTES};

#[test]
fn the_body_limit_defaults_to_sixteen_mebibytes() {
    assert_eq!(DEFAULT_MAX_BODY_BYTES, 16 * 1024 * 1024);
    assert_eq!(parse_body_limit(None), DEFAULT_MAX_BODY_BYTES);
    assert_eq!(parse_body_limit(Some("garbage")), DEFAULT_MAX_BODY_BYTES);
    assert_eq!(parse_body_limit(Some("0")), DEFAULT_MAX_BODY_BYTES);
}

#[test]
fn the_body_limit_takes_a_larger_override() {
    assert_eq!(parse_body_limit(Some(" 1073741824 ")), 1_073_741_824);
}

#[tokio::test]
async fn a_body_over_the_limit_is_refused_and_read_under_a_raised_limit() {
    let body = vec![7u8; DEFAULT_MAX_BODY_BYTES + 1];
    let refused = read_production_body(axum::body::Body::from(body.clone()), DEFAULT_MAX_BODY_BYTES).await;
    assert_eq!(refused.err(), Some(http::StatusCode::PAYLOAD_TOO_LARGE));
    let read = read_production_body(axum::body::Body::from(body.clone()), 2 * DEFAULT_MAX_BODY_BYTES).await;
    assert_eq!(read.map(|bytes| bytes.len()), Ok(body.len()));
}
