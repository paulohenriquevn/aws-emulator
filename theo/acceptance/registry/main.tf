# AM6: one private ECR repository per platform image, immutable, scanned, with a lifecycle policy.
resource "aws_iam_role" "ci_publish" {
  name = "theo-acceptance-ci-publish"
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Action = "sts:AssumeRole", Principal = { Service = "codebuild.amazonaws.com" } }]
  })
}

module "ecr" {
  source  = "terraform-aws-modules/ecr/aws"
  version = "3.2.1"

  for_each = toset(["theo-api", "theo-cloud", "theo-tenant-operator"])

  repository_name                   = each.key
  repository_image_tag_mutability   = "IMMUTABLE"
  repository_image_scan_on_push     = true
  repository_encryption_type        = "KMS"
  repository_read_write_access_arns = [aws_iam_role.ci_publish.arn]
  repository_lifecycle_policy = jsonencode({
    rules = [{
      rulePriority = 1
      description  = "expire untagged images after 14 days"
      selection    = { tagStatus = "untagged", countType = "sinceImagePushed", countUnit = "days", countNumber = 14 }
      action       = { type = "expire" }
    }]
  })
}

output "repository_urls" { value = { for k, m in module.ecr : k => m.repository_url } }
