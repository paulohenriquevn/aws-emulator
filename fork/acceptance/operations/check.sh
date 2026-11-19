#!/usr/bin/env bash
# What the operations cluster's controllers do at run time, which Terraform never calls.
set -euo pipefail

# The CloudFormation stack created the role it declares, as an IAM role anyone can read.
role=$(terraform output -raw controllers_role)
arn=$(terraform output -raw controllers_role_arn)
read_arn=$(aws iam get-role --role-name "$role" --query 'Role.Arn' --output text)
if [[ $read_arn != "$arn" ]]; then
  echo "check: IAM reports role $role as '$read_arn', the stack output says '$arn'" >&2
  exit 1
fi
aws iam get-instance-profile --instance-profile-name "$role" --query 'InstanceProfile.Roles[0].RoleName' --output text \
  | grep -qx "$role" || { echo "check: the stack's instance profile does not hold $role" >&2; exit 1; }

# EventBridge delivers a matching event to the SQS target.
queue=$(terraform output -raw events_queue_url)
marker="probe-$RANDOM-$RANDOM"
failed=$(aws events put-events --entries \
  "[{\"Source\":\"acceptance.probe\",\"DetailType\":\"probe\",\"Detail\":\"{\\\"marker\\\":\\\"$marker\\\"}\"}]" \
  --query 'FailedEntryCount' --output text)
[[ $failed == 0 ]] || { echo "check: PutEvents failed $failed entries" >&2; exit 1; }
body=$(aws sqs receive-message --queue-url "$queue" --wait-time-seconds 10 --max-number-of-messages 1 \
  --query 'Messages[0].Body' --output text)
if [[ $body != *"$marker"* ]]; then
  echo "check: the queue did not receive the probe event; got: $body" >&2
  exit 1
fi

# The AWS Load Balancer Controller's Shield call answers for an account with no subscription.
state=$(aws shield get-subscription-state --query 'SubscriptionState' --output text)
[[ $state == INACTIVE ]] || { echo "check: Shield subscription state is '$state', not INACTIVE" >&2; exit 1; }

echo "check: stack role $arn, event delivered to SQS, Shield $state"
