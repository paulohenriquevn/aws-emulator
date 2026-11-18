#!/usr/bin/env bash
# What a dashboard deploy needs from CloudFront beyond what Terraform reads back: the
# distribution serves the security headers policy, and a cache invalidation completes.
set -euo pipefail

distribution=$(terraform output -raw cdn_id)
policy=$(terraform output -raw headers_policy_id)

attached=$(aws cloudfront get-distribution-config --id "$distribution" \
  --query 'DistributionConfig.DefaultCacheBehavior.ResponseHeadersPolicyId' --output text)
if [[ $attached != "$policy" ]]; then
  echo "check: the distribution's default behavior carries policy '$attached', not '$policy'" >&2
  exit 1
fi

csp=$(aws cloudfront get-response-headers-policy --id "$policy" \
  --query 'ResponseHeadersPolicy.ResponseHeadersPolicyConfig.SecurityHeadersConfig.ContentSecurityPolicy.ContentSecurityPolicy' \
  --output text)
expected="default-src 'self'; frame-ancestors 'none'; base-uri 'self'"
if [[ $csp != "$expected" ]]; then
  echo "check: the policy's CSP is '$csp', not '$expected'" >&2
  exit 1
fi

invalidation=$(aws cloudfront create-invalidation --distribution-id "$distribution" \
  --paths '/*' --query 'Invalidation.Id' --output text)
aws cloudfront wait invalidation-completed --distribution-id "$distribution" --id "$invalidation"
echo "check: policy attached, CSP served, invalidation $invalidation completed"
