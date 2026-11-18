# AM7: Route53 zone, ACM certificate validated by DNS, a public ALB with WAF, and the
# dashboard served from a private S3 origin through CloudFront (OAC).
module "zone" {
  source  = "terraform-aws-modules/route53/aws"
  version = "6.5.1"

  name = "acceptance.aws-emulator.dev"
}

module "acm" {
  source  = "terraform-aws-modules/acm/aws"
  version = "6.3.1"

  domain_name               = "acceptance.aws-emulator.dev"
  subject_alternative_names = ["*.acceptance.aws-emulator.dev"]
  zone_id                   = module.zone.id
  validation_method         = "DNS"
  wait_for_validation       = true
}

module "alb" {
  source  = "terraform-aws-modules/alb/aws"
  version = "10.5.1"

  name                       = "acceptance"
  vpc_id                     = module.vpc.vpc_id
  subnets                    = module.vpc.public_subnets
  enable_deletion_protection = false

  security_group_ingress_rules = {
    https = { from_port = 443, to_port = 443, ip_protocol = "tcp", cidr_ipv4 = "0.0.0.0/0" }
  }
  security_group_egress_rules = {
    vpc = { ip_protocol = "-1", cidr_ipv4 = module.vpc.vpc_cidr_block }
  }

  listeners = {
    https = {
      port            = 443
      protocol        = "HTTPS"
      certificate_arn = module.acm.acm_certificate_arn
      forward         = { target_group_key = "api" }
    }
  }
  target_groups = {
    api = {
      protocol          = "HTTP"
      port              = 8080
      target_type       = "ip"
      create_attachment = false
      health_check      = { path = "/health/ready", matcher = "200" }
    }
  }
}

resource "aws_wafv2_web_acl" "edge" {
  name  = "acceptance"
  scope = "REGIONAL"
  default_action {
    allow {}
  }
  rule {
    name     = "aws-common"
    priority = 1
    override_action {
      none {}
    }
    statement {
      managed_rule_group_statement {
        name        = "AWSManagedRulesCommonRuleSet"
        vendor_name = "AWS"
      }
    }
    visibility_config {
      cloudwatch_metrics_enabled = true
      metric_name                = "aws-common"
      sampled_requests_enabled   = true
    }
  }
  visibility_config {
    cloudwatch_metrics_enabled = true
    metric_name                = "acceptance"
    sampled_requests_enabled   = true
  }
}

resource "aws_wafv2_web_acl_association" "alb" {
  resource_arn = module.alb.arn
  web_acl_arn  = aws_wafv2_web_acl.edge.arn
}

module "dashboard_bucket" {
  source  = "terraform-aws-modules/s3-bucket/aws"
  version = "5.17.0"

  bucket        = "acceptance-dashboard"
  force_destroy = true
}

# The security headers the dashboard's nginx sets, which the SaaS edge must reproduce
# once the dashboard is served from S3 (no nginx in the path).
resource "aws_cloudfront_response_headers_policy" "dashboard" {
  name = "acceptance-dashboard-security"

  security_headers_config {
    content_security_policy {
      content_security_policy = "default-src 'self'; frame-ancestors 'none'; base-uri 'self'"
      override                = true
    }
    strict_transport_security {
      access_control_max_age_sec = 31536000
      include_subdomains         = true
      preload                    = true
      override                   = true
    }
    content_type_options {
      override = true
    }
    frame_options {
      frame_option = "DENY"
      override     = true
    }
    referrer_policy {
      referrer_policy = "strict-origin-when-cross-origin"
      override        = true
    }
  }
}

module "cdn" {
  source  = "terraform-aws-modules/cloudfront/aws"
  version = "6.7.1"

  comment             = "fork acceptance dashboard"
  default_root_object = "index.html"
  aliases             = ["app.acceptance.aws-emulator.dev"]

  origin_access_control = {
    dashboard = {
      description      = "dashboard bucket"
      origin_type      = "s3"
      signing_behavior = "always"
      signing_protocol = "sigv4"
    }
  }
  origin = {
    dashboard = {
      domain_name               = module.dashboard_bucket.s3_bucket_bucket_regional_domain_name
      origin_access_control_key = "dashboard"
    }
  }
  default_cache_behavior = {
    target_origin_id           = "dashboard"
    viewer_protocol_policy     = "redirect-to-https"
    cache_policy_name          = "Managed-CachingOptimized"
    response_headers_policy_id = aws_cloudfront_response_headers_policy.dashboard.id
  }
  viewer_certificate = {
    acm_certificate_arn = module.acm.acm_certificate_arn
    ssl_support_method  = "sni-only"
  }
}

output "alb_dns_name" { value = module.alb.dns_name }
output "cdn_domain_name" { value = module.cdn.cloudfront_distribution_domain_name }
output "cdn_id" { value = module.cdn.cloudfront_distribution_id }
output "headers_policy_id" { value = aws_cloudfront_response_headers_policy.dashboard.id }
