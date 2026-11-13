# AM3: crosscutting KMS key, GitHub OIDC deploy role, a secret container, an SSM parameter.
data "aws_caller_identity" "current" {}

module "kms" {
  source  = "terraform-aws-modules/kms/aws"
  version = "4.2.2"

  description             = "theo acceptance crosscutting key"
  deletion_window_in_days = 7
  enable_key_rotation     = true
  aliases                 = ["theo/acceptance"]
}

resource "aws_iam_openid_connect_provider" "github" {
  url            = "https://token.actions.githubusercontent.com"
  client_id_list = ["sts.amazonaws.com"]
}

module "deploy_role" {
  source  = "terraform-aws-modules/iam/aws//modules/iam-role"
  version = "6.8.2"

  name = "theo-acceptance-deploy"
  trust_policy_permissions = {
    GithubOidc = {
      actions = ["sts:AssumeRoleWithWebIdentity"]
      principals = [{ type = "Federated", identifiers = [aws_iam_openid_connect_provider.github.arn] }]
      condition = [{
        test     = "StringLike"
        variable = "token.actions.githubusercontent.com:sub"
        values   = ["repo:usetheoai/*:ref:refs/heads/main"]
      }]
    }
  }
  policies = { ReadOnly = "arn:aws:iam::aws:policy/ReadOnlyAccess" }
}

module "secret" {
  source  = "terraform-aws-modules/secrets-manager/aws"
  version = "2.2.0"

  name                    = "theo/acceptance/database"
  kms_key_id              = module.kms.key_arn
  recovery_window_in_days = 0
  ignore_secret_changes   = true
  secret_string           = jsonencode({ placeholder = "injected-out-of-band" })
}

module "parameter" {
  source  = "terraform-aws-modules/ssm-parameter/aws"
  version = "2.1.2"

  name   = "/theo/acceptance/engine-url"
  value  = "http://theo-api.theo-system:8080"
  type   = "String"
  tier   = "Standard"
}

output "key_arn" { value = module.kms.key_arn }
output "role_arn" { value = module.deploy_role.arn }
output "account_id" { value = data.aws_caller_identity.current.account_id }
