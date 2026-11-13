# A minimal VPC for fixtures that need one but do not test it (network/ does).
module "vpc" {
  source  = "terraform-aws-modules/vpc/aws"
  version = "6.7.3"

  name             = "theo-${basename(path.cwd)}"
  cidr             = "10.30.0.0/16"
  azs              = ["us-east-1a", "us-east-1b"]
  public_subnets   = ["10.30.0.0/24", "10.30.1.0/24"]
  private_subnets  = ["10.30.10.0/24", "10.30.11.0/24"]
  database_subnets = ["10.30.20.0/24", "10.30.21.0/24"]

  create_database_subnet_group = true
  enable_dns_hostnames         = true
  manage_default_network_acl   = false
}
