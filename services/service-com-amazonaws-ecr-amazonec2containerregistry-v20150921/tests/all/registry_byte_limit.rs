//! The registry-wide ceiling on the bytes the emulated ECR holds. A build that pushes a
//! full Node.js stage with its registry cache sends more than the 512 MiB default, so a
//! deployment that hosts tenant images raises it with EMULATOR_ECR_MAX_STORED_BYTES.

use service_com_amazonaws_ecr_amazonec2containerregistry_v20150921::state::{
    parse_registry_byte_limit, DEFAULT_MAX_REGISTRY_CALLER_BYTES,
};

#[test]
fn the_registry_byte_limit_defaults_to_512_mebibytes() {
    assert_eq!(DEFAULT_MAX_REGISTRY_CALLER_BYTES, 512 * 1024 * 1024);
    assert_eq!(parse_registry_byte_limit(None), DEFAULT_MAX_REGISTRY_CALLER_BYTES);
    assert_eq!(parse_registry_byte_limit(Some("garbage")), DEFAULT_MAX_REGISTRY_CALLER_BYTES);
    assert_eq!(parse_registry_byte_limit(Some("0")), DEFAULT_MAX_REGISTRY_CALLER_BYTES);
}

#[test]
fn the_registry_byte_limit_takes_a_larger_override() {
    assert_eq!(parse_registry_byte_limit(Some(" 8589934592 ")), 8_589_934_592);
}
