# AM5: EKS control plane + a managed node group + IRSA, as ROADMAP_AWS declares it.
module "eks" {
  source  = "terraform-aws-modules/eks/aws"
  version = "21.29.0"

  name               = "theo-acceptance"
  kubernetes_version = "1.34"
  vpc_id             = module.vpc.vpc_id
  subnet_ids         = module.vpc.private_subnets

  endpoint_public_access                   = true
  enable_irsa                              = true
  enable_cluster_creator_admin_permissions = true

  addons = {
    coredns    = {}
    kube-proxy = {}
    vpc-cni    = { before_compute = true }
  }

  eks_managed_node_groups = {
    general = {
      instance_types = ["m7g.large"]
      ami_type       = "AL2023_ARM_64_STANDARD"
      min_size       = 1
      max_size       = 3
      desired_size   = 2
    }
  }
}

output "cluster_endpoint" { value = module.eks.cluster_endpoint }
output "oidc_provider_arn" { value = module.eks.oidc_provider_arn }
