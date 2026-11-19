// SPDX-License-Identifier: CC0-1.0

//! fork: `DeleteReplicationGroup`.
//!
//! "By default, this operation deletes the entire replication group, including the
//! primary/primaries and all of the read replicas" (`DeleteReplicationGroup`). The
//! Terraform AWS provider destroys an `aws_elasticache_replication_group` with it and then
//! waits for `DescribeReplicationGroups` to answer `ReplicationGroupNotFoundFault`, so a
//! scaffolded 501 left every Terraform stack holding a replication group undestroyable.

use crate::harness::{params, Emulator, PAST_TRANSITION};

async fn create_group(emulator: &Emulator, id: &str, extra: &[(&str, &str)]) {
    let mut pairs = params(&[
        ("ReplicationGroupId", id),
        ("ReplicationGroupDescription", "a group to delete"),
    ]);
    pairs.extend(params(extra));
    let created = emulator.call("CreateReplicationGroup", &pairs).await;
    assert_eq!(created.status, 200, "case {id}: create must answer 200, got {}", created.body);
}

#[tokio::test]
async fn deleting_a_group_removes_it_and_every_member_cluster() {
    let emulator = Emulator::new();
    // (requested id, stored id, the form pairs, the member clusters the group is made of)
    let cases: [(&str, &str, &[(&str, &str)], &[&str]); 3] = [
        ("flat-del", "flat-del", &[("NumCacheClusters", "2")], &["flat-del-001", "flat-del-002"]),
        ("shard-del", "shard-del", &[("NumNodeGroups", "2")], &["shard-del-0001-001", "shard-del-0002-001"]),
        ("Mixed-Del", "mixed-del", &[], &["mixed-del-001"]),
    ];
    for (id, _, extra, _) in cases {
        create_group(&emulator, id, extra).await;
    }
    std::thread::sleep(PAST_TRANSITION);

    for (id, stored, _, members) in cases {
        let deleted = emulator
            .call("DeleteReplicationGroup", &params(&[("ReplicationGroupId", id)]))
            .await;
        assert_eq!(deleted.status, 200, "case {id}: delete must answer 200, got {}", deleted.body);
        assert_eq!(
            deleted.text("ReplicationGroup/ReplicationGroupId"),
            Some(stored),
            "case {id}: the response must report the deleted group, got {}",
            deleted.body
        );
        assert_eq!(
            deleted.text("ReplicationGroup/Status"),
            Some("deleting"),
            "case {id}: the deleted group must report deleting, got {}",
            deleted.body
        );

        let described = emulator
            .call("DescribeReplicationGroups", &params(&[("ReplicationGroupId", stored)]))
            .await;
        assert_eq!(
            (described.status, described.error_code()),
            (404, "ReplicationGroupNotFoundFault"),
            "case {id}: a deleted group must no longer be described, got {}",
            described.body
        );
        for member in members {
            let cluster = emulator
                .call("DescribeCacheClusters", &params(&[("CacheClusterId", member)]))
                .await;
            assert_eq!(
                (cluster.status, cluster.error_code()),
                (404, "CacheClusterNotFound"),
                "case {id}/{member}: a member cluster leaves with its group, got {}",
                cluster.body
            );
        }
    }
}

#[tokio::test]
async fn a_delete_this_service_cannot_carry_out_is_refused_and_keeps_the_group() {
    let emulator = Emulator::new();

    let unknown = emulator
        .call("DeleteReplicationGroup", &params(&[("ReplicationGroupId", "no-such-group")]))
        .await;
    assert_eq!(
        (unknown.status, unknown.error_code()),
        (404, "ReplicationGroupNotFoundFault"),
        "case an unknown group: got {}",
        unknown.body
    );

    // Still `creating`: "The requested replication group is not in the available state."
    create_group(&emulator, "young-del", &[]).await;
    let young = emulator
        .call("DeleteReplicationGroup", &params(&[("ReplicationGroupId", "young-del")]))
        .await;
    assert_eq!(
        (young.status, young.error_code()),
        (400, "InvalidReplicationGroupState"),
        "case a creating group: got {}",
        young.body
    );
    std::thread::sleep(PAST_TRANSITION);

    // A final snapshot this emulator cannot take is refused rather than claimed.
    let snapshot = emulator
        .call(
            "DeleteReplicationGroup",
            &params(&[
                ("ReplicationGroupId", "young-del"),
                ("FinalSnapshotIdentifier", "young-del-final"),
            ]),
        )
        .await;
    assert_eq!(
        (snapshot.status, snapshot.error_code()),
        (400, "SnapshotFeatureNotSupportedFault"),
        "case a final snapshot: got {}",
        snapshot.body
    );

    let kept = emulator
        .call("DescribeReplicationGroups", &params(&[("ReplicationGroupId", "young-del")]))
        .await;
    assert_eq!(
        (kept.status, kept.text("ReplicationGroup/Status")),
        (200, Some("available")),
        "case refused deletes: the group must still be there, got {}",
        kept.body
    );
}
