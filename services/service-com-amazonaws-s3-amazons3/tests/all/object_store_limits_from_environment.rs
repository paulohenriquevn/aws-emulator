// SPDX-License-Identifier: CC0-1.0

//! The four ceilings on what the in-memory object store holds: objects per bucket, bytes
//! per bucket, objects across every bucket and bytes across every bucket. They bound this
//! process's memory rather than any S3 quota, so a long-running deployment (log chunks
//! written for days, build artefacts of a few GiB) raises them with
//! `EMULATOR_S3_MAX_OBJECTS_PER_BUCKET`, `EMULATOR_S3_MAX_OBJECT_BYTES_PER_BUCKET`,
//! `EMULATOR_S3_MAX_OBJECTS_TOTAL` and `EMULATOR_S3_MAX_OBJECT_BYTES_TOTAL`, the way
//! `EMULATOR_ECR_MAX_STORED_BYTES` raises the ECR registry's budget.
//!
//! The ceilings are read once per process, so the suite that proves a raised ceiling runs
//! in a child copy of this test binary started with the variables set. The default
//! ceilings refusing a write past them is pinned by `object_and_tagging_roundtrip`.

use std::process::Command;
use std::sync::Arc;

use service_com_amazonaws_s3_amazons3::state::{
    max_object_bytes_per_bucket, max_object_bytes_total, max_objects_per_bucket, max_objects_total,
    parse_object_store_limit, ComAmazonawsS3Amazons3State, ObjectWrite,
    DEFAULT_MAX_OBJECTS_PER_BUCKET, DEFAULT_MAX_OBJECTS_TOTAL, DEFAULT_MAX_OBJECT_BYTES_PER_BUCKET,
    DEFAULT_MAX_OBJECT_BYTES_TOTAL, MAX_OBJECTS_PER_BUCKET_ENV, MAX_OBJECTS_TOTAL_ENV,
    MAX_OBJECT_BYTES_PER_BUCKET_ENV, MAX_OBJECT_BYTES_TOTAL_ENV,
};
use service_com_amazonaws_s3_amazons3::EmulatedService;
use smithy_server_runtime::MultiServiceRouter;

use super::object_and_tagging_roundtrip::{
    bucket, object_retaining, one_byte_object, put_object, text,
};

const MIB: usize = 1024 * 1024;

/// The raised object ceiling the child process runs with, per bucket and in total: above
/// both defaults, so a write the defaults refuse is accepted, and low enough to reach.
const RAISED_OBJECTS: usize = 10_500;

/// The raised byte budget the child process runs with, per bucket and in total: above
/// both defaults (64 MiB and 128 MiB).
const RAISED_BYTES: usize = 160 * MIB;

/// The child test's path inside this binary, as libtest names it.
const RAISED_CEILINGS_CHILD: &str =
    "object_store_limits_from_environment::raised_ceilings_take_writes_past_the_defaults";

#[test]
fn the_object_store_ceilings_default_to_the_values_the_emulator_always_had() {
    assert_eq!(DEFAULT_MAX_OBJECTS_PER_BUCKET, 1_000);
    assert_eq!(DEFAULT_MAX_OBJECT_BYTES_PER_BUCKET, 64 * MIB);
    assert_eq!(DEFAULT_MAX_OBJECTS_TOTAL, 10_000);
    assert_eq!(DEFAULT_MAX_OBJECT_BYTES_TOTAL, 128 * MIB);
    assert_eq!(
        MAX_OBJECTS_PER_BUCKET_ENV,
        "EMULATOR_S3_MAX_OBJECTS_PER_BUCKET"
    );
    assert_eq!(
        MAX_OBJECT_BYTES_PER_BUCKET_ENV,
        "EMULATOR_S3_MAX_OBJECT_BYTES_PER_BUCKET"
    );
    assert_eq!(MAX_OBJECTS_TOTAL_ENV, "EMULATOR_S3_MAX_OBJECTS_TOTAL");
    assert_eq!(
        MAX_OBJECT_BYTES_TOTAL_ENV,
        "EMULATOR_S3_MAX_OBJECT_BYTES_TOTAL"
    );
}

#[test]
fn an_unset_unparseable_or_zero_value_keeps_the_default() {
    for (case, raw) in [
        ("unset", None),
        ("not a number", Some("garbage")),
        ("zero", Some("0")),
        ("negative", Some("-5")),
        ("empty", Some("")),
    ] {
        assert_eq!(
            parse_object_store_limit(raw, DEFAULT_MAX_OBJECTS_PER_BUCKET),
            DEFAULT_MAX_OBJECTS_PER_BUCKET,
            "case {case:?}: the default must stand"
        );
    }
}

#[test]
fn a_positive_value_overrides_the_default() {
    assert_eq!(
        parse_object_store_limit(Some(" 8589934592 "), DEFAULT_MAX_OBJECT_BYTES_TOTAL),
        8_589_934_592
    );
    assert_eq!(
        parse_object_store_limit(Some("250000"), DEFAULT_MAX_OBJECTS_PER_BUCKET),
        250_000
    );
}

/// Starts this test binary again with every ceiling raised and runs
/// [`raised_ceilings_take_writes_past_the_defaults`] in it, since the ceilings this
/// process already read cannot change.
#[test]
fn the_environment_raises_every_object_store_ceiling() {
    let binary = std::env::current_exe().expect("the test binary's own path");
    let output = Command::new(binary)
        .args([RAISED_CEILINGS_CHILD, "--exact", "--ignored", "--nocapture"])
        .env(MAX_OBJECTS_PER_BUCKET_ENV, RAISED_OBJECTS.to_string())
        .env(MAX_OBJECTS_TOTAL_ENV, RAISED_OBJECTS.to_string())
        .env(MAX_OBJECT_BYTES_PER_BUCKET_ENV, RAISED_BYTES.to_string())
        .env(MAX_OBJECT_BYTES_TOTAL_ENV, RAISED_BYTES.to_string())
        .output()
        .expect("the child test binary must start");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "the child run with raised ceilings must pass exactly one test, got {}\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
}

/// Run only by [`the_environment_raises_every_object_store_ceiling`], in a process whose
/// environment raises every ceiling.
#[tokio::test]
#[ignore = "runs in a child process that the_environment_raises_every_object_store_ceiling starts with the ceilings raised"]
async fn raised_ceilings_take_writes_past_the_defaults() {
    if std::env::var_os(MAX_OBJECTS_PER_BUCKET_ENV).is_none() {
        eprintln!(
            "skipped: {MAX_OBJECTS_PER_BUCKET_ENV} is unset, so the ceilings were not raised"
        );
        return;
    }
    assert_eq!(max_objects_per_bucket(), RAISED_OBJECTS);
    assert_eq!(max_objects_total(), RAISED_OBJECTS);
    assert_eq!(max_object_bytes_per_bucket(), RAISED_BYTES);
    assert_eq!(max_object_bytes_total(), RAISED_BYTES);

    // Object count: one bucket takes more objects than either default count ceiling
    // allows, and a write past the raised ceiling is still refused.
    let engine: Arc<engine::InMemoryEngine> = Arc::new(engine::InMemoryEngine::new());
    let mut router = MultiServiceRouter::new();
    router.set_request_authentication(false);
    router.register_service(EmulatedService::new(Arc::clone(&engine)));
    bucket(&router, "many-objects").await;
    {
        let state = engine.state::<ComAmazonawsS3Amazons3State>();
        let mut store = state.lock();
        for index in 0..=DEFAULT_MAX_OBJECTS_TOTAL {
            let outcome =
                store.put_object("many-objects", &format!("chunk-{index}"), one_byte_object());
            assert_eq!(
                outcome,
                Some(ObjectWrite::Created),
                "case {index}: a write past the default object ceilings must be recorded \
                 once the environment raised them, got {outcome:?}"
            );
        }
    }
    // Over the wire, as the deployment that hit the ceiling wrote: PutObject answers 200.
    let accepted = put_object(&router, "many-objects", "probe/plain", b"probe", &[]).await;
    assert_eq!(
        accepted.status,
        200,
        "PutObject past the default object ceilings must be accepted, got {} with body {:?}",
        accepted.status,
        text(&accepted)
    );
    {
        let state = engine.state::<ComAmazonawsS3Amazons3State>();
        let mut store = state.lock();
        let mut index = DEFAULT_MAX_OBJECTS_TOTAL + 1;
        while store
            .bucket("many-objects")
            .map_or(0, |held| held.stored_versions())
            < RAISED_OBJECTS
        {
            let outcome =
                store.put_object("many-objects", &format!("chunk-{index}"), one_byte_object());
            assert_eq!(
                outcome,
                Some(ObjectWrite::Created),
                "case {index}: below the raised ceiling"
            );
            index += 1;
        }
        let outcome = store.put_object(
            "many-objects",
            "one-past-the-raised-ceiling",
            one_byte_object(),
        );
        assert!(
            matches!(
                outcome,
                Some(ObjectWrite::ObjectCountReached | ObjectWrite::TotalObjectCountReached)
            ),
            "a fresh key past the raised ceiling of {RAISED_OBJECTS} objects must be refused, \
             got {outcome:?}"
        );
    }

    // Bytes, in a fresh store so the objects above do not count: one bucket takes more
    // bytes than either default budget, and a write past the raised budget is refused.
    let engine: Arc<engine::InMemoryEngine> = Arc::new(engine::InMemoryEngine::new());
    let mut router = MultiServiceRouter::new();
    router.set_request_authentication(false);
    router.register_service(EmulatedService::new(Arc::clone(&engine)));
    bucket(&router, "many-bytes").await;
    {
        let state = engine.state::<ComAmazonawsS3Amazons3State>();
        let mut store = state.lock();
        let object_bytes = 48 * MIB;
        for index in 0..3 {
            let outcome = store.put_object(
                "many-bytes",
                &format!("artefact-{index}"),
                object_retaining(object_bytes),
            );
            assert_eq!(
                outcome,
                Some(ObjectWrite::Created),
                "case {index}: {} MiB in one bucket is past both default byte budgets and \
                 must be recorded once the environment raised them, got {outcome:?}",
                (index + 1) * 48
            );
        }
        let outcome = store.put_object(
            "many-bytes",
            "one-past-the-raised-budget",
            object_retaining(object_bytes),
        );
        assert!(
            matches!(
                outcome,
                Some(
                    ObjectWrite::ByteBudgetReached { .. }
                        | ObjectWrite::TotalByteBudgetReached { .. }
                )
            ),
            "a write past the raised budget of {RAISED_BYTES} bytes must be refused, got {outcome:?}"
        );
    }
}
