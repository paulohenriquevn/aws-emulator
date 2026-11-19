# AM4: RDS PostgreSQL behind RDS Proxy, an ElastiCache replication group, an S3 bucket.
resource "aws_security_group" "db" {
  name   = "acceptance-db"
  vpc_id = module.vpc.vpc_id
  ingress {
    from_port   = 5432
    to_port     = 5432
    protocol    = "tcp"
    cidr_blocks = [module.vpc.vpc_cidr_block]
  }
}

module "db" {
  source  = "terraform-aws-modules/rds/aws"
  version = "7.2.2"

  identifier           = "acceptance"
  engine               = "postgres"
  engine_version       = "17"
  family               = "postgres17"
  major_engine_version = "17"
  instance_class       = "db.t4g.micro"
  allocated_storage    = 20

  db_name                     = "app"
  username                    = "app"
  manage_master_user_password = true
  port                        = 5432

  db_subnet_group_name   = module.vpc.database_subnet_group_name
  vpc_security_group_ids = [aws_security_group.db.id]

  backup_retention_period = 7
  deletion_protection     = false
  skip_final_snapshot     = true
  parameters              = [{ name = "rds.force_ssl", value = "1" }]
}

module "proxy" {
  source  = "terraform-aws-modules/rds-proxy/aws"
  version = "4.4.0"

  name                   = "acceptance"
  iam_role_name          = "acceptance-proxy"
  vpc_subnet_ids         = module.vpc.private_subnets
  vpc_security_group_ids = [aws_security_group.db.id]
  engine_family          = "POSTGRESQL"
  require_tls            = true
  target_db_instance     = true
  db_instance_identifier = module.db.db_instance_identifier
  auth = {
    "app" = {
      description = "app master credentials"
      secret_arn  = module.db.db_instance_master_user_secret_arn
    }
  }
}

module "cache" {
  source  = "terraform-aws-modules/elasticache/aws"
  version = "1.11.2"

  replication_group_id       = "acceptance"
  engine                     = "valkey"
  engine_version             = "8.0"
  node_type                  = "cache.t4g.micro"
  num_cache_clusters         = 2
  transit_encryption_enabled = true
  vpc_id                     = module.vpc.vpc_id
  subnet_ids                 = module.vpc.private_subnets
  create_parameter_group     = true
  parameter_group_family     = "valkey8"
  security_group_rules = {
    ingress_vpc = { cidr_ipv4 = module.vpc.vpc_cidr_block }
  }
}

module "bucket" {
  source  = "terraform-aws-modules/s3-bucket/aws"
  version = "5.17.0"

  bucket        = "acceptance-tenant-storage"
  force_destroy = true
  versioning    = { enabled = true }
  server_side_encryption_configuration = {
    rule = { apply_server_side_encryption_by_default = { sse_algorithm = "aws:kms" } }
  }
}

output "db_endpoint" { value = module.db.db_instance_endpoint }
output "proxy_endpoint" { value = module.proxy.proxy_endpoint }
output "cache_endpoint" { value = module.cache.replication_group_primary_endpoint_address }
