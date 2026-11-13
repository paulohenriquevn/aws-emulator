// SPDX-License-Identifier: CC0-1.0

//! The Amazon EKS optimized Amazon Linux 2023 AMI hierarchies of the published `/aws/service`
//! namespace, end to end over the front door.
//!
//! Amazon EKS publishes, per Kubernetes version and AMI type, a `recommended` path whose
//! `image_id` leaf names the node AMI (Amazon EKS User Guide, "Retrieve recommended Amazon
//! Linux AMI IDs") and whose `release_version` leaf is the AMI release a managed node group
//! pins. `terraform-aws-modules/eks` reads `release_version` for every AL2023 node group, so a
//! missing leaf fails the whole plan. The rows below are the release `v20261107` of
//! `awslabs/amazon-eks-ami`, the data this emulator records.

use super::public_parameter_catalog::{
    describe_image, element, emulator, is_ami_id, names, read_published, recursive_listing,
};

/// `(path, release version, architecture)` for a sample that varies the Kubernetes version,
/// the architecture and the variant.
const CASES: &[(&str, &str, &str)] = &[
    (
        "/aws/service/eks/optimized-ami/1.34/amazon-linux-2023/arm64/standard/recommended",
        "1.34.12-20261107",
        "arm64",
    ),
    (
        "/aws/service/eks/optimized-ami/1.31/amazon-linux-2023/x86_64/standard/recommended",
        "1.31.14-20261107",
        "x86_64",
    ),
    (
        "/aws/service/eks/optimized-ami/1.37/amazon-linux-2023/x86_64/nvidia/recommended",
        "1.37.1-20261107",
        "x86_64",
    ),
    (
        "/aws/service/eks/optimized-ami/1.36/amazon-linux-2023/arm64/nvidia/recommended",
        "1.36.5-20261107",
        "arm64",
    ),
    (
        "/aws/service/eks/optimized-ami/1.35/amazon-linux-2023/x86_64/neuron/recommended",
        "1.35.9-20261107",
        "x86_64",
    ),
];

#[tokio::test]
async fn the_release_version_leaf_reports_the_recorded_ami_release() {
    let router = emulator();

    for (path, release_version, _) in CASES {
        let name = format!("{path}/release_version");

        let parameter = read_published(&router, &name).await;

        assert_eq!(
            parameter["Value"].as_str(),
            Some(*release_version),
            "case {name}: got {parameter}"
        );
    }
}

#[tokio::test]
async fn the_image_id_leaf_names_an_image_ec2_describes_with_the_path_architecture() {
    let router = emulator();

    for (path, _, architecture) in CASES {
        let case = format!("{path}/image_id");
        let image_id = read_published(&router, &case).await["Value"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(is_ami_id(&image_id), "case {case}: expected an AMI id, got {image_id:?}");

        let (status, body) = describe_image(&router, &image_id).await;

        assert_eq!(status, 200, "case {case}: DescribeImages {image_id}: {body}");
        assert_eq!(
            element(&body, "architecture").as_deref(),
            Some(*architecture),
            "case {case}: {body}"
        );
    }
}

#[tokio::test]
async fn a_recommended_path_enumerates_exactly_its_two_leaves() {
    let router = emulator();
    let (path, _, _) = CASES[0];

    let listed = names(&recursive_listing(&router, path, path).await);

    assert_eq!(
        listed,
        vec![format!("{path}/image_id"), format!("{path}/release_version")],
        "the recommended path must publish image_id and release_version only"
    );
}
