# What the operations cluster asks of AWS besides the stacks Terraform builds:
# Cluster API's IAM bootstrap (a CloudFormation stack, as `clusterawsadm bootstrap iam`
# creates it), CAPA's instance-state events (EventBridge -> SQS), the Shield state the
# AWS Load Balancer Controller reads, and the on-demand prices OpenCost reads.

locals {
  controllers_role = "acceptance-capa-controllers"
}

resource "aws_cloudformation_stack" "capa_bootstrap" {
  name         = "acceptance-capa-bootstrap"
  capabilities = ["CAPABILITY_NAMED_IAM"]
  template_body = jsonencode({
    AWSTemplateFormatVersion = "2010-09-09"
    Description              = "Cluster API Provider AWS controller role"
    Resources = {
      ControllersRole = {
        Type = "AWS::IAM::Role"
        Properties = {
          RoleName = local.controllers_role
          AssumeRolePolicyDocument = {
            Version = "2012-10-17"
            Statement = [{
              Effect    = "Allow"
              Principal = { Service = "ec2.amazonaws.com" }
              Action    = "sts:AssumeRole"
            }]
          }
        }
      }
      ControllersInstanceProfile = {
        Type = "AWS::IAM::InstanceProfile"
        Properties = {
          InstanceProfileName = local.controllers_role
          Roles               = [{ Ref = "ControllersRole" }]
        }
      }
    }
    Outputs = {
      ControllersRoleArn = { Value = { "Fn::GetAtt" = ["ControllersRole", "Arn"] } }
    }
  })
}

resource "aws_sqs_queue" "capa_events" {
  name                      = "acceptance-capa-events"
  message_retention_seconds = 3600
}

resource "aws_sqs_queue_policy" "capa_events" {
  queue_url = aws_sqs_queue.capa_events.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "events.amazonaws.com" }
      Action    = "sqs:SendMessage"
      Resource  = aws_sqs_queue.capa_events.arn
    }]
  })
}

# The rule CAPA creates for an instance's state changes.
resource "aws_cloudwatch_event_rule" "instance_state" {
  name = "acceptance-capa-instance-state"
  event_pattern = jsonencode({
    source        = ["aws.ec2"]
    "detail-type" = ["EC2 Instance State-change Notification"]
  })
}

resource "aws_cloudwatch_event_target" "instance_state" {
  rule = aws_cloudwatch_event_rule.instance_state.name
  arn  = aws_sqs_queue.capa_events.arn
}

# A rule check.sh can trigger itself: PutEvents may not use an aws.* source.
resource "aws_cloudwatch_event_rule" "delivery_probe" {
  name          = "acceptance-delivery-probe"
  event_pattern = jsonencode({ source = ["acceptance.probe"] })
}

resource "aws_cloudwatch_event_target" "delivery_probe" {
  rule = aws_cloudwatch_event_rule.delivery_probe.name
  arn  = aws_sqs_queue.capa_events.arn
}

data "aws_pricing_product" "node" {
  service_code = "AmazonEC2"
  filters {
    field = "instanceType"
    value = "t3.medium"
  }
  filters {
    field = "regionCode"
    value = "us-east-1"
  }
  filters {
    field = "operatingSystem"
    value = "Linux"
  }
  filters {
    field = "tenancy"
    value = "Shared"
  }
  filters {
    field = "preInstalledSw"
    value = "NA"
  }
  filters {
    field = "capacitystatus"
    value = "Used"
  }
}

output "controllers_role" { value = local.controllers_role }
output "controllers_role_arn" { value = aws_cloudformation_stack.capa_bootstrap.outputs["ControllersRoleArn"] }
output "events_queue_url" { value = aws_sqs_queue.capa_events.id }
output "node_price_sku" { value = jsondecode(data.aws_pricing_product.node.result).product.sku }
