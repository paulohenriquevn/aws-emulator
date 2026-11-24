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

/// One layer upload may grow to Amazon ECR's published maximum layer size, 52,000 MiB,
/// or to the registry-wide ceiling when that is smaller, since every byte of it is held
/// in this process's memory. A 211 MB Node.js base layer was refused under the old fixed
/// 128 MiB per-upload cap (#25).
#[test]
fn a_layer_upload_is_bounded_by_the_ecr_quota_and_the_registry_ceiling() {
    use service_com_amazonaws_ecr_amazonec2containerregistry_v20150921::state::{
        layer_upload_limit, ECR_MAX_LAYER_BYTES,
    };
    assert_eq!(ECR_MAX_LAYER_BYTES, 52_000 * 1024 * 1024);
    assert_eq!(layer_upload_limit(DEFAULT_MAX_REGISTRY_CALLER_BYTES), DEFAULT_MAX_REGISTRY_CALLER_BYTES);
    assert_eq!(layer_upload_limit(8 << 30), 8 << 30);
    assert_eq!(layer_upload_limit(usize::MAX), ECR_MAX_LAYER_BYTES);
}
