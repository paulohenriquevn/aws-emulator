// SPDX-License-Identifier: CC0-1.0

//! The Amazon EKS optimized Amazon Linux 2023 node images, and the public Systems Manager
//! hierarchy Amazon EKS publishes about each one.
//!
//! Amazon EKS publishes, per Kubernetes version and per AMI type, a hierarchy at
//! `/aws/service/eks/optimized-ami/<kubernetes-version>/amazon-linux-2023/<arch>/<variant>/recommended`
//! (Amazon EKS User Guide, "Retrieve recommended Amazon Linux AMI IDs"). Clients read two
//! leaves of it:
//!
//! - `image_id`, the id of the recommended node AMI, which the guide documents and a
//!   self-managed node group launches;
//! - `release_version`, the AMI release a managed node group pins
//!   (`CreateNodegroup$ReleaseVersion`), which `terraform-aws-modules/eks` reads for every
//!   AL2023 AMI type.
//!
//! One source of truth, as for every other published alias: the `image_id` leaf is the
//! `Image$PublicSsmParameterName` of an image of the published catalog, so the id a read
//! reports is an AMI the account can describe and launch. The `release_version` leaf is
//! published beside it by [`ComAmazonawsEc2Amazonec2State::public_ssm_parameter_values`],
//! and only while that image is visible.
//!
//! The data is recorded, not derived: the Kubernetes versions, the release versions and the
//! AMI names below are those of the `awslabs/amazon-eks-ami` release `v20261107`, published
//! 2026-11-07, which ships every AL2023 AMI type for Kubernetes 1.31 through 1.37.

use super::{published_amazon_image, ImageSpec, BOOT_MODE_UEFI, BOOT_MODE_UEFI_PREFERRED, IMDS_SUPPORT_V2};

/// The level of the public namespace the EKS optimized AMI hierarchies are published at.
pub const EKS_OPTIMIZED_AMI_NAMESPACE: &str = "/aws/service/eks/optimized-ami";

/// The leaf a hierarchy's image id is published under.
const IMAGE_ID_LEAF: &str = "image_id";

/// The leaf a hierarchy's AMI release version is published under.
pub const RELEASE_VERSION_LEAF: &str = "release_version";

/// The build date every node AMI of the release carries in its name (`v<date>`).
const EKS_AMI_BUILD: &str = "20261107";

/// The `CreationDate` of the release's images: the instant the release was published.
const EKS_AMI_CREATION_DATE: &str = "2026-11-07T22:43:45.000Z";

/// `(Kubernetes version, release version)` for every Kubernetes version the release ships.
const EKS_RELEASES: &[(&str, &str)] = &[
    ("1.31", "1.31.14-20261107"),
    ("1.32", "1.32.13-20261107"),
    ("1.33", "1.33.13-20261107"),
    ("1.34", "1.34.12-20261107"),
    ("1.35", "1.35.9-20261107"),
    ("1.36", "1.36.5-20261107"),
    ("1.37", "1.37.1-20261107"),
];

/// `(architecture, variant)` for every AL2023 AMI type the release ships for each version:
/// `AL2023_x86_64_STANDARD`, `AL2023_ARM_64_STANDARD`, `AL2023_x86_64_NVIDIA`,
/// `AL2023_ARM_64_NVIDIA` and `AL2023_x86_64_NEURON`.
const EKS_AMI_TYPES: &[(&str, &str)] = &[
    ("x86_64", "standard"),
    ("arm64", "standard"),
    ("x86_64", "nvidia"),
    ("arm64", "nvidia"),
    ("x86_64", "neuron"),
];

/// One published EKS optimized AMI hierarchy.
pub struct EksOptimizedHierarchy {
    /// The `.../recommended` path the hierarchy is published at.
    pub path: String,
    /// The AMI release version published under [`RELEASE_VERSION_LEAF`].
    pub release_version: &'static str,
    /// The name of the AMI the hierarchy recommends.
    pub image_name: String,
    /// The architecture of that AMI.
    pub architecture: &'static str,
}

impl EksOptimizedHierarchy {
    /// The public parameter the hierarchy's image id is published under.
    pub fn image_id_parameter(&self) -> String {
        format!("{}/{IMAGE_ID_LEAF}", self.path)
    }
}

/// Every EKS optimized AMI hierarchy the catalog publishes, one per Kubernetes version and
/// AMI type.
pub fn eks_optimized_hierarchies() -> Vec<EksOptimizedHierarchy> {
    EKS_RELEASES
        .iter()
        .flat_map(|&(kubernetes, release_version)| {
            EKS_AMI_TYPES.iter().map(move |&(architecture, variant)| EksOptimizedHierarchy {
                path: format!(
                    "{EKS_OPTIMIZED_AMI_NAMESPACE}/{kubernetes}/amazon-linux-2023/{architecture}/{variant}/recommended"
                ),
                release_version,
                image_name: format!(
                    "amazon-eks-node-al2023-{architecture}-{variant}-{kubernetes}-v{EKS_AMI_BUILD}"
                ),
                architecture,
            })
        })
        .collect()
}

/// The images those hierarchies recommend, as rows of the published catalog
/// ([`super::published_image_specs`]).
///
/// The description AWS gives these images is not recorded here, so none is reported. A node
/// image is no Free Tier image.
pub fn published_eks_optimized_specs() -> Vec<ImageSpec> {
    eks_optimized_hierarchies()
        .into_iter()
        .map(|hierarchy| {
            let boot_mode = if hierarchy.architecture == "arm64" {
                BOOT_MODE_UEFI
            } else {
                BOOT_MODE_UEFI_PREFERRED
            };
            ImageSpec {
                boot_mode: Some(boot_mode.to_string()),
                imds_support: Some(IMDS_SUPPORT_V2.to_string()),
                free_tier_eligible: false,
                public_ssm_parameter_name: Some(hierarchy.image_id_parameter()),
                ..published_amazon_image(
                    &hierarchy.image_name,
                    hierarchy.architecture,
                    EKS_AMI_CREATION_DATE,
                )
            }
        })
        .collect()
}
