# The monthly cost guardrail theo-infra-modules/billing/budget declares.
resource "aws_budgets_budget" "monthly" {
  name         = "theo-acceptance-monthly"
  budget_type  = "COST"
  limit_amount = "500"
  limit_unit   = "USD"
  time_unit    = "MONTHLY"

  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 80
    threshold_type             = "PERCENTAGE"
    notification_type          = "FORECASTED"
    subscriber_email_addresses = ["ops@example.com"]
  }
}

output "budget_name" { value = aws_budgets_budget.monthly.name }
