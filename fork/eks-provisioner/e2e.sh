#!/usr/bin/env bash
# SPDX-License-Identifier: CC0-1.0
#
# End to end check of the EKS provisioner, driven only through the AWS APIs a Terraform
# apply uses:
#   1. the emulator runs with EMULATOR_EKS_CONTROL_PLANE=external, and the provisioner
#      beside it;
#   2. a VPC, two subnets, an EKS cluster and a managed node group are created with the
#      AWS CLI, and `aws eks wait cluster-active` returns once the provisioner registered
#      the kind control plane;
#   3. `aws eks update-kubeconfig` writes a kubeconfig whose user runs `aws eks get-token`,
#      and kubectl with it lists the node group's instances as Ready nodes, each with its
#      instance's providerID and address;
#   4. a pod whose service account is annotated with an IRSA role, and that sets no AWS
#      variable of its own, is that role for `aws sts get-caller-identity`: the pod
#      identity webhook gives it the role and the token, and the provisioner's webhook
#      the emulator's endpoint and CA; a kube-system pod gets no endpoint;
#   5. a principal with no access entry is refused, and deleting the cluster deletes its
#      kind cluster.
#
# Usage: fork/eks-provisioner/e2e.sh <emulator-binary> <provisioner-binary>
# Needs docker, kind, kubectl, aws (CLI v2), curl and jq. It touches only the kind cluster
# and the Docker network it creates, so it runs beside another emulator and provisioner
# on the same host: set E2E_PORT and E2E_WEBHOOK_PORT to ports they do not use.
set -euo pipefail

BIN=$(realpath "${1:?the emulator binary}")
PROVISIONER=$(realpath "${2:?the provisioner binary}")
WORK=${E2E_WORK:-$(mktemp -d)}
mkdir -p "$WORK"
LOG=$WORK/e2e.log
PORT=${E2E_PORT:-9597}
REGION=us-east-1
CLUSTER=e2e
NODEGROUP=workers
NODEGROUP_SIZE=2
VERSION=1.34
EMULATOR_NETWORK=aws-emulator-e2e
EMULATOR_CIDR=172.30.0.0/16
GATEWAY=172.30.0.1
VPC_CIDR=10.42.0.0/16
AWS_CLI_IMAGE=public.ecr.aws/aws-cli/aws-cli:2.37.8

say() { printf '[e2e] %s\n' "$*"; }
fail() { printf '[e2e] FAIL: %s\n' "$*" >&2; exit 1; }
wait_for() { local tries=$1; shift; for _ in $(seq "$tries"); do "$@" >/dev/null 2>&1 && return 0; sleep 2; done; return 1; }

cleanup() {
  [[ -n ${PROVISIONER_PID:-} ]] && kill "$PROVISIONER_PID" 2>/dev/null || true
  kind get clusters 2>/dev/null | grep -qx "eks-$REGION-$CLUSTER" && { kind delete cluster --name "eks-$REGION-$CLUSTER" >>"$LOG" 2>&1 || true; }
  [[ -n ${EMU_PID:-} ]] && kill "$EMU_PID" 2>/dev/null || true
  if [[ -n ${VPC:-} ]]; then
    for network in $(docker network ls --filter "label=aws-emulator.vpc-id=$VPC" --format '{{.Name}}'); do
      docker network inspect "$network" --format '{{range .IPAM.Config}}{{.Subnet}}{{end}}' | grep -qx "$VPC_CIDR" && { docker network rm "$network" >>"$LOG" 2>&1 || true; }
    done
  fi
  docker network rm "$EMULATOR_NETWORK" >>"$LOG" 2>&1 || true
}
trap cleanup EXIT

# --- 1. The emulator and the provisioner ---------------------------------------------------
docker network inspect "$EMULATOR_NETWORK" >/dev/null 2>&1 ||
  docker network create "$EMULATOR_NETWORK" --subnet "$EMULATOR_CIDR" --gateway "$GATEWAY" >>"$LOG"
EMU_HOME=$WORK/home
mkdir -p "$EMU_HOME"
EMULATOR_EKS_CONTROL_PLANE=external EMULATOR_BIND_ADDRESS=$GATEWAY HOME=$EMU_HOME \
  "$BIN" -b "$GATEWAY" -p "$PORT" --telemetry off >"$WORK/emulator.log" 2>&1 &
EMU_PID=$!
EMU_DIR=$EMU_HOME/.aws/emulator/$PORT
export AWS_ENDPOINT_URL=https://$GATEWAY:$PORT
wait_for 30 curl -sf --cacert "$EMU_DIR/ca.pem" "$AWS_ENDPOINT_URL/__admin/health" || fail "the emulator did not answer; see $WORK/emulator.log"
export AWS_CA_BUNDLE=$EMU_DIR/ca.pem AWS_SHARED_CREDENTIALS_FILE=$EMU_DIR/emulator_credentials AWS_CONFIG_FILE=$EMU_DIR/config
export AWS_REGION=$REGION AWS_DEFAULT_REGION=$REGION AWS_EC2_METADATA_DISABLED=true AWS_PAGER=

"$PROVISIONER" --regions "$REGION" --state-dir "$WORK/provisioner" --listen ":${E2E_WEBHOOK_PORT:-9598}" --interval 2s >"$WORK/provisioner.log" 2>&1 &
PROVISIONER_PID=$!
say "emulator at $AWS_ENDPOINT_URL, provisioner pid $PROVISIONER_PID"

# --- 2. The cluster, as Terraform creates it -----------------------------------------------
VPC=$(aws ec2 create-vpc --cidr-block "$VPC_CIDR" --query Vpc.VpcId --output text)
SUBNETS=()
for spec in 10.42.0.0/19:${REGION}a 10.42.32.0/19:${REGION}b; do
  SUBNETS+=("$(aws ec2 create-subnet --vpc-id "$VPC" --cidr-block "${spec%%:*}" --availability-zone "${spec##*:}" --query Subnet.SubnetId --output text)")
done
trust() { jq -cn --arg principal "$1" '{Version:"2012-10-17",Statement:[{Effect:"Allow",Principal:{Service:$principal},Action:"sts:AssumeRole"}]}'; }
CLUSTER_ROLE=$(aws iam create-role --role-name "$CLUSTER-cluster" --assume-role-policy-document "$(trust eks.amazonaws.com)" --query Role.Arn --output text)
NODE_ROLE=$(aws iam create-role --role-name "$CLUSTER-nodes" --assume-role-policy-document "$(trust ec2.amazonaws.com)" --query Role.Arn --output text)
aws eks create-cluster --name "$CLUSTER" --kubernetes-version "$VERSION" --role-arn "$CLUSTER_ROLE" \
  --resources-vpc-config "subnetIds=$(IFS=,; echo "${SUBNETS[*]}")" >>"$LOG"
[[ $(aws eks describe-cluster --name "$CLUSTER" --query cluster.status --output text) == CREATING ]] || fail "the cluster is not CREATING right after create-cluster"
aws eks wait cluster-active --name "$CLUSTER" || fail "the cluster did not turn ACTIVE; see $WORK/provisioner.log"
ENDPOINT=$(aws eks describe-cluster --name "$CLUSTER" --query cluster.endpoint --output text)
say "cluster $CLUSTER ACTIVE at $ENDPOINT"

aws eks create-nodegroup --cluster-name "$CLUSTER" --nodegroup-name "$NODEGROUP" --node-role "$NODE_ROLE" \
  --subnets "${SUBNETS[@]}" --instance-types t3.large --labels role=general \
  --scaling-config "minSize=1,maxSize=$NODEGROUP_SIZE,desiredSize=$NODEGROUP_SIZE" >>"$LOG"
aws eks wait nodegroup-active --cluster-name "$CLUSTER" --nodegroup-name "$NODEGROUP"

# --- 3. A kubeconfig from the EKS API, and the nodes behind it ----------------------------
export KUBECONFIG=$WORK/kubeconfig
aws eks update-kubeconfig --name "$CLUSTER" --kubeconfig "$KUBECONFIG" >>"$LOG"
kubectl get --raw /version >/dev/null || fail "kubectl with the token of aws eks get-token was refused"
ready_nodes() {
  kubectl get nodes -l "eks.amazonaws.com/nodegroup=$NODEGROUP,role=general" -o json |
    jq -e --argjson size "$NODEGROUP_SIZE" '[.items[] | select(.status.conditions[] | select(.type == "Ready" and .status == "True"))] | length == $size'
}
wait_for 90 ready_nodes || { kubectl get nodes -o wide; fail "the node group's instances did not become $NODEGROUP_SIZE Ready nodes"; }
expected=$(aws ec2 describe-instances --filters "Name=tag:eks:nodegroup-name,Values=$NODEGROUP" "Name=instance-state-name,Values=running" \
  --query 'Reservations[].Instances[].[Placement.AvailabilityZone,InstanceId,PrivateIpAddress]' --output text | awk '{print "aws:///"$1"/"$2" "$3}' | sort)
actual=$(kubectl get nodes -l "eks.amazonaws.com/nodegroup=$NODEGROUP" -o json |
  jq -r '.items[] | .spec.providerID + " " + (.status.addresses[] | select(.type == "InternalIP") | .address)' | sort)
[[ $expected == "$actual" ]] || fail "nodes do not match the instances: expected [$expected], got [$actual]"
say "nodes are the node group's instances: $(echo "$actual" | tr '\n' ' ')"

# --- 4. IRSA, and the emulator's endpoint and CA, with nothing set in the pod -------------
ISSUER=$(aws eks describe-cluster --name "$CLUSTER" --query cluster.identity.oidc.issuer --output text)
PROVIDER=${ISSUER#https://}
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
aws iam create-open-id-connect-provider --url "$ISSUER" --client-id-list sts.amazonaws.com \
  --thumbprint-list 9e99a48a9960b14926bb7f3b02e22da2b0ab7280 >>"$LOG"
IRSA_ROLE=$(aws iam create-role --role-name "$CLUSTER-irsa" --query Role.Arn --output text --assume-role-policy-document "$(
  jq -cn --arg provider "$PROVIDER" --arg federated "arn:aws:iam::$ACCOUNT:oidc-provider/$PROVIDER" \
    '{Version:"2012-10-17",Statement:[{Effect:"Allow",Principal:{Federated:$federated},Action:"sts:AssumeRoleWithWebIdentity",
      Condition:{StringEquals:{($provider+":sub"):"system:serviceaccount:irsa:irsa-probe",($provider+":aud"):"sts.amazonaws.com"}}}]}')")
kubectl create namespace irsa >>"$LOG"
kubectl -n irsa create serviceaccount irsa-probe >>"$LOG"
kubectl -n irsa annotate serviceaccount irsa-probe "eks.amazonaws.com/role-arn=$IRSA_ROLE" >>"$LOG"
kubectl apply -f - >>"$LOG" <<POD
apiVersion: v1
kind: Pod
metadata: {name: irsa-probe, namespace: irsa}
spec:
  serviceAccountName: irsa-probe
  restartPolicy: Never
  containers:
  - name: aws
    image: $AWS_CLI_IMAGE
    args: [sts, get-caller-identity, --query, Arn, --output, text]
POD
kubectl -n irsa wait pod irsa-probe --for=jsonpath='{.status.phase}'=Succeeded --timeout=300s >>"$LOG" 2>&1 ||
  { kubectl -n irsa describe pod irsa-probe; kubectl -n irsa logs irsa-probe; fail "the IRSA probe did not succeed"; }
answer=$(kubectl -n irsa logs irsa-probe)
[[ $answer == "arn:aws:sts::$ACCOUNT:assumed-role/$CLUSTER-irsa/"* ]] || fail "the IRSA probe is $answer, not role $IRSA_ROLE"
say "irsa: a pod with no AWS variable of its own is $answer"
system_env=$(kubectl -n kube-system get pods -l app=pod-identity-webhook -o jsonpath='{.items[*].spec.containers[*].env[*].name}')
[[ $system_env != *AWS_ENDPOINT_URL* ]] || fail "a kube-system pod got the emulator's endpoint"
say "kube-system pods keep their own environment"

# --- 5. Access entries decide, and deletion tears down -------------------------------------
OUTSIDER=$(aws iam create-role --role-name outsider --assume-role-policy-document \
  "$(jq -cn '{Version:"2012-10-17",Statement:[{Effect:"Allow",Principal:{AWS:"arn:aws:iam::000000000000:root"},Action:"sts:AssumeRole"}]}')" --query Role.Arn --output text)
read -r key secret session < <(aws sts assume-role --role-arn "$OUTSIDER" --role-session-name e2e \
  --query 'Credentials.[AccessKeyId,SecretAccessKey,SessionToken]' --output text)
if AWS_ACCESS_KEY_ID=$key AWS_SECRET_ACCESS_KEY=$secret AWS_SESSION_TOKEN=$session AWS_SHARED_CREDENTIALS_FILE=/dev/null \
  kubectl get --raw /version >>"$LOG" 2>&1; then
  fail "a role with no access entry was let in"
fi
say "a role with no access entry is refused"

# aws eks get-token presigns for 60 seconds and kubectl reuses the token for 14 minutes:
# STS and Amazon EKS accept it for 15 minutes from X-Amz-Date. The token is used for the
# first time after its presign expiry, so no API server cache has seen it.
TOKEN=$(aws eks get-token --cluster-name "$CLUSTER" --query status.token --output text)
aws eks describe-cluster --name "$CLUSTER" --query cluster.certificateAuthority.data --output text | base64 -d >"$WORK/cluster-ca.pem"
sleep 70
status=$(curl -s -o /dev/null -w '%{http_code}' --cacert "$WORK/cluster-ca.pem" -H "Authorization: Bearer $TOKEN" "$ENDPOINT/version")
[[ $status == 200 ]] || fail "a token first used 70 seconds after aws eks get-token was refused with $status"
say "a token is accepted after its 60 second presign expiry"

aws eks delete-nodegroup --cluster-name "$CLUSTER" --nodegroup-name "$NODEGROUP" >>"$LOG"
aws eks wait nodegroup-deleted --cluster-name "$CLUSTER" --nodegroup-name "$NODEGROUP"
aws eks delete-cluster --name "$CLUSTER" >>"$LOG"
aws eks wait cluster-deleted --name "$CLUSTER"
wait_for 60 bash -c "! kind get clusters 2>/dev/null | grep -qx eks-$REGION-$CLUSTER" || fail "the kind cluster outlived the EKS cluster"
say "deleting the cluster deleted its kind cluster"
say "PASS"
