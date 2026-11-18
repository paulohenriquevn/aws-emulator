# A VPC with NAT gateways, the network a platform's workloads run in.
module "vpc" {
  source  = "terraform-aws-modules/vpc/aws"
  version = "6.7.3"

  name             = "acceptance"
  cidr             = "10.20.0.0/16"
  azs              = ["us-east-1a", "us-east-1b"]
  public_subnets   = ["10.20.0.0/24", "10.20.1.0/24"]
  private_subnets  = ["10.20.10.0/24", "10.20.11.0/24"]
  database_subnets = ["10.20.20.0/24", "10.20.21.0/24"]

  create_database_subnet_group  = true
  enable_nat_gateway            = true
  single_nat_gateway            = true
  enable_dns_hostnames          = true
  enable_dns_support            = true
  manage_default_network_acl    = true
  manage_default_security_group = true
}

output "vpc_id" { value = module.vpc.vpc_id }
output "nat_gateway_ids" { value = module.vpc.natgw_ids }
output "database_subnet_group" { value = module.vpc.database_subnet_group_name }
