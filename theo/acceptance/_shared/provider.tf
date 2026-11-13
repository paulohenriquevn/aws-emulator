# Shared by every fixture through a symlink. The endpoint, credentials and CA come
# from the environment run.sh exports (AWS_ENDPOINT_URL, AWS_SHARED_CREDENTIALS_FILE,
# AWS_CONFIG_FILE, AWS_CA_BUNDLE), so no fixture can reach real AWS by accident: the
# runner refuses to start without them.
terraform {
  required_version = ">= 1.10"
  required_providers {
    aws = { source = "hashicorp/aws", version = "~> 6.28" }
  }
}

provider "aws" {
  region = "us-east-1"
  # The emulator serves every service on one IP endpoint, so S3 must use path-style
  # addressing; a bucket name in the host would never reach it.
  s3_use_path_style = true
  default_tags {
    tags = { Fixture = basename(path.cwd), ManagedBy = "theo-acceptance" }
  }
}
