// SPDX-License-Identifier: CC0-1.0

//! fork: the EC2 instances a managed node group runs.
//!
//! "Amazon EKS managed node groups automate the provisioning and lifecycle management of
//! nodes (Amazon EC2 instances)" (Amazon EKS User Guide, "Simplify node lifecycle with managed
//! node groups"). `CreateNodegroup` launches `scalingConfig.desiredSize` instances in the
//! shared EC2, one per call, spread over the node group's subnets in order, and tags each one
//! the way Amazon EKS tags its nodes: `eks:cluster-name`, `eks:nodegroup-name` and
//! `kubernetes.io/cluster/<cluster>` = `owned`. The image is the Amazon EKS optimized AL2023
//! AMI of the node group's Kubernetes version and architecture, named through its public
//! Systems Manager parameter (`resolve:ssm:`), which EC2 resolves itself. `DeleteNodegroup`
//! terminates them.
//!
//! What is not modelled: the Auto Scaling group Amazon EKS creates (the instances are
//! launched directly), and a scaling change by `UpdateNodegroupConfig`, which launches or
//! terminates nothing.
//!
//! Both run in a task of their own, after the call answered: the handler holds the store's
//! guard, which no await may cross. A failure is logged as an error naming the node group.

/// The tag naming the cluster a node belongs to.
pub const CLUSTER_NAME_TAG: &str = "eks:cluster-name";

/// The tag naming the node group a node belongs to.
pub const NODEGROUP_NAME_TAG: &str = "eks:nodegroup-name";

/// What a node group's instances are launched with.
#[derive(Debug, Clone)]
pub struct NodeSpec {
    pub cluster_name: String,
    pub nodegroup_name: String,
    pub desired: i32,
    pub instance_type: String,
    pub subnets: Vec<String>,
    pub ami_type: String,
    pub kubernetes_version: String,
    pub launch_template: Option<super::LaunchTemplateSpecification>,
}

/// The EC2 client a node group's instances are launched and terminated through: the
/// in-process callback into this emulator.
pub struct NodeLauncher {
    ec2: ::aws_sdk_ec2::Client,
}

/// The launcher for a request signed for `region`, or `None` when the region names no
/// endpoint (see [`super::peers`]).
pub fn node_launcher(ctx: &smithy_server_runtime::OperationContext<'_>, region: &str) -> Option<NodeLauncher> {
    let endpoint = super::peers::peer_endpoint("ec2", region)?;
    Some(NodeLauncher {
        ec2: callback_client!(aws_sdk_ec2, ctx, endpoint, region),
    })
}

/// The `resolve:ssm:` image id of the Amazon EKS optimized AL2023 AMI for `ami_type` at
/// `kubernetes_version`: `arm64` for an ARM AMI type, `x86_64` otherwise.
#[must_use]
pub fn optimized_image(ami_type: &str, kubernetes_version: &str) -> String {
    let arch = if ami_type.contains("ARM") { "arm64" } else { "x86_64" };
    format!(
        "resolve:ssm:/aws/service/eks/optimized-ami/{kubernetes_version}/amazon-linux-2023/{arch}/standard/recommended/image_id"
    )
}

/// The subnet the `index`-th node is launched into: the node group's subnets in turn.
#[must_use]
pub fn subnet_for(subnets: &[String], index: usize) -> Option<&str> {
    (!subnets.is_empty()).then(|| subnets[index % subnets.len()].as_str())
}

fn tag(key: &str, value: &str) -> ::aws_sdk_ec2::types::Tag {
    ::aws_sdk_ec2::types::Tag::builder().key(key).value(value).build()
}

impl NodeLauncher {
    /// Launch the node group's instances, in the background.
    pub fn launch(self, spec: NodeSpec) {
        tokio::spawn(async move {
            let desired = usize::try_from(spec.desired).unwrap_or(0);
            let image = optimized_image(&spec.ami_type, &spec.kubernetes_version);
            for index in 0..desired {
                let mut request = self
                    .ec2
                    .run_instances()
                    .image_id(&image)
                    .instance_type(::aws_sdk_ec2::types::InstanceType::from(spec.instance_type.as_str()))
                    .min_count(1)
                    .max_count(1)
                    .set_subnet_id(subnet_for(&spec.subnets, index).map(str::to_string))
                    .tag_specifications(
                        ::aws_sdk_ec2::types::TagSpecification::builder()
                            .resource_type(::aws_sdk_ec2::types::ResourceType::Instance)
                            .tags(tag(CLUSTER_NAME_TAG, &spec.cluster_name))
                            .tags(tag(NODEGROUP_NAME_TAG, &spec.nodegroup_name))
                            .tags(tag(&format!("kubernetes.io/cluster/{}", spec.cluster_name), "owned"))
                            .tags(tag("Name", &spec.nodegroup_name))
                            .build(),
                    );
                if let Some(template) = &spec.launch_template {
                    request = request.launch_template(
                        ::aws_sdk_ec2::types::LaunchTemplateSpecification::builder()
                            .set_launch_template_id(template.id.clone())
                            .set_launch_template_name(template.name.clone())
                            .set_version(template.version.clone())
                            .build(),
                    );
                }
                match request.send().await {
                    Ok(output) => {
                        let ids: Vec<String> = output
                            .instances()
                            .iter()
                            .filter_map(|instance| instance.instance_id().map(str::to_string))
                            .collect();
                        ::tracing::info!(cluster = %spec.cluster_name, nodegroup = %spec.nodegroup_name, ?ids, "launched a node of the node group");
                    }
                    Err(error) => {
                        ::tracing::error!(
                            cluster = %spec.cluster_name,
                            nodegroup = %spec.nodegroup_name,
                            error = %::aws_smithy_types::error::display::DisplayErrorContext(&error),
                            "a node of the node group could not be launched in EC2"
                        );
                        return;
                    }
                }
            }
        });
    }

    /// Terminate every instance tagged as a node of the node group, in the background.
    pub fn terminate(self, cluster_name: String, nodegroup_name: String) {
        tokio::spawn(async move {
            let filter = |name: &str, values: &[&str]| {
                ::aws_sdk_ec2::types::Filter::builder()
                    .name(name)
                    .set_values(Some(values.iter().map(|value| (*value).to_string()).collect()))
                    .build()
            };
            let described = self
                .ec2
                .describe_instances()
                .filters(filter(&format!("tag:{CLUSTER_NAME_TAG}"), &[&cluster_name]))
                .filters(filter(&format!("tag:{NODEGROUP_NAME_TAG}"), &[&nodegroup_name]))
                .filters(filter("instance-state-name", &["pending", "running", "stopping", "stopped"]))
                .send()
                .await;
            let ids: Vec<String> = match described {
                Ok(output) => output
                    .reservations()
                    .iter()
                    .flat_map(|reservation| reservation.instances())
                    .filter_map(|instance| instance.instance_id().map(str::to_string))
                    .collect(),
                Err(error) => {
                    ::tracing::error!(
                        cluster = %cluster_name,
                        nodegroup = %nodegroup_name,
                        error = %::aws_smithy_types::error::display::DisplayErrorContext(&error),
                        "the node group's instances could not be listed in EC2"
                    );
                    return;
                }
            };
            if ids.is_empty() {
                return;
            }
            if let Err(error) = self.ec2.terminate_instances().set_instance_ids(Some(ids)).send().await {
                ::tracing::error!(
                    cluster = %cluster_name,
                    nodegroup = %nodegroup_name,
                    error = %::aws_smithy_types::error::display::DisplayErrorContext(&error),
                    "the node group's instances could not be terminated in EC2"
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_image_is_the_optimized_al2023_ami_of_the_version_and_architecture() {
        assert_eq!(
            optimized_image("AL2023_x86_64_STANDARD", "1.34"),
            "resolve:ssm:/aws/service/eks/optimized-ami/1.34/amazon-linux-2023/x86_64/standard/recommended/image_id"
        );
        assert!(optimized_image("AL2023_ARM_64_STANDARD", "1.34").contains("/arm64/"));
    }

    #[test]
    fn nodes_are_spread_over_the_subnets_in_turn() {
        let subnets = vec!["subnet-a".to_string(), "subnet-b".to_string()];
        assert_eq!(subnet_for(&subnets, 0), Some("subnet-a"));
        assert_eq!(subnet_for(&subnets, 1), Some("subnet-b"));
        assert_eq!(subnet_for(&subnets, 2), Some("subnet-a"));
        assert_eq!(subnet_for(&[], 0), None);
    }
}
