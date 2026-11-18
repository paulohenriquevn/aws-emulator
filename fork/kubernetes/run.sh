#!/usr/bin/env bash
# Run real in-cluster AWS controllers and real workload identity against the emulator, with
# a kind cluster standing in for an EKS cluster's control plane.
#
#   1. The emulator listens on the Docker network kind uses, so pods reach it, and its CA is
#      mounted into every pod that calls it.
#   2. An EKS cluster is created in the emulator. kind's API server signs service account
#      tokens as that cluster's OIDC issuer, and its public key set is registered for the
#      issuer (PUT /_fork/eks/clusters/<name>/service-account-issuer-keys).
#   3. amazon-eks-pod-identity-webhook injects IRSA (annotated service accounts) and EKS Pod
#      Identity (associated service accounts) into pods; the EKS Pod Identity Agent serves
#      Pod Identity credentials on 169.254.170.23 from eks-auth:AssumeRoleForPodIdentity.
#   4. Each check below must pass:
#        irsa                  an annotated pod is the IRSA role (sts get-caller-identity)
#        pod-identity          an associated pod gets the role's credentials from the agent
#        load-balancer         the AWS Load Balancer Controller (IRSA) turns an Ingress into
#                              an ALB in the emulator, with the node registered as a target,
#                              and a request to the ALB's name reaches the echo pod
#        external-secrets      External Secrets (IRSA) copies a Secrets Manager secret into
#                              a Kubernetes Secret
#        opencost              OpenCost prices the node from the emulator's EC2 price list
#
# Usage: fork/kubernetes/run.sh <emulator-binary> [check ...]
# Needs docker, kind, kubectl, helm, aws (CLI v2), curl, jq, openssl and git.
# KUBERNETES_WORK (default: a new temp dir), KUBERNETES_PORT (default 9597), KEEP_CLUSTER=1
# to leave the kind cluster and the emulator running afterwards.
# Exit status: 0 when every selected check passes, 1 otherwise.
set -euo pipefail

BIN=${1:-}
if [[ -z $BIN || ! -x $BIN ]]; then
  echo "usage: $0 <emulator-binary> [check ...]  (not an executable: ${BIN:-<none>})" >&2
  exit 2
fi
BIN=$(realpath "$BIN")
shift
CHECKS=("$@")
[[ ${#CHECKS[@]} -gt 0 ]] || CHECKS=(irsa pod-identity load-balancer external-secrets opencost)

HERE=$(cd "$(dirname "$0")" && pwd)
WORK=${KUBERNETES_WORK:-$(mktemp -d)}
mkdir -p "$WORK"
WORK=$(realpath "$WORK")
PORT=${KUBERNETES_PORT:-9597}
REGION=us-east-1
CLUSTER=operations
KIND_CLUSTER=aws-emulator
NODE_INSTANCE_TYPE=t3.medium
# The kind node's Docker network IS the EKS cluster's VPC: the node instance's private
# address is the node container's own, so a load balancer target the emulator dials is
# reachable from the host. kind hands the node an address of IP_RANGE, outside the first
# four and the last address of the subnet that holds it, which EC2 reserves.
KIND_NETWORK=${KIND_NETWORK:-aws-emulator-kind}
VPC_CIDR=${KIND_NETWORK_CIDR:-172.29.0.0/16}
NODE_SUBNET_CIDR=${VPC_CIDR%.*.*}.0.0/17
OTHER_SUBNET_CIDR=${VPC_CIDR%.*.*}.128.0/17
NODE_IP_RANGE=${VPC_CIDR%.*.*}.1.0/24
NETWORK_GATEWAY=${VPC_CIDR%.*.*}.0.1
POD_CIDR=10.244.0.0/16
LOG=$WORK/run.log

# Pinned versions of everything the cluster runs.
LBC_CHART_VERSION=3.6.3
ESO_CHART_VERSION=2.15.0
OPENCOST_CHART_VERSION=2.5.34
POD_IDENTITY_AGENT_COMMIT=ff8532e7579cc6691d6319e818f13bdfefeeb83c
WEBHOOK_IMAGE=amazon/amazon-eks-pod-identity-webhook:latest
AWS_CLI_IMAGE=public.ecr.aws/aws-cli/aws-cli:2.37.8
FILE_SERVER_IMAGE=nginx:1.29.3-alpine

unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN AWS_PROFILE AWS_DEFAULT_PROFILE
export KUBECONFIG=$WORK/kubeconfig

say() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*" | tee -a "$LOG" >&2; }
fail() { say "FAILED: $*"; exit 1; }

# Wait up to $1 seconds for "$2..." to succeed.
wait_for() {
  local seconds=$1; shift
  for _ in $(seq "$seconds"); do
    if "$@" >>"$LOG" 2>&1; then return 0; fi
    sleep 1
  done
  return 1
}

cleanup() {
  if [[ ${KEEP_CLUSTER:-0} == 1 ]]; then
    say "kept: kind cluster $KIND_CLUSTER (KUBECONFIG=$KUBECONFIG), emulator pid ${EMU_PID:-none} at $AWS_ENDPOINT_URL"
    return
  fi
  kind delete cluster --name "$KIND_CLUSTER" >>"$LOG" 2>&1 || true
  if [[ ${CREATED_NETWORK:-0} == 1 ]]; then docker network rm "$KIND_NETWORK" >>"$LOG" 2>&1 || true; fi
  if [[ -n ${EMU_PID:-} ]]; then kill "$EMU_PID" 2>/dev/null || true; wait "$EMU_PID" 2>/dev/null || true; fi
}
trap cleanup EXIT

# --- 1. The emulator, on the Docker network kind uses ------------------------------------

if ! docker network inspect "$KIND_NETWORK" >/dev/null 2>&1; then
  docker network create "$KIND_NETWORK" --driver bridge --subnet "$VPC_CIDR" --ip-range "$NODE_IP_RANGE" \
    --gateway "$NETWORK_GATEWAY" -o com.docker.network.bridge.enable_ip_masquerade=true >>"$LOG" || fail "creating the Docker network $KIND_NETWORK"
  CREATED_NETWORK=1
fi
export KIND_EXPERIMENTAL_DOCKER_NETWORK=$KIND_NETWORK
GATEWAY=$(docker network inspect "$KIND_NETWORK" -f '{{range .IPAM.Config}}{{.Gateway}} {{end}}' | tr ' ' '\n' | grep -E '^[0-9]+(\.[0-9]+){3}$' | head -1)
[[ -n $GATEWAY ]] || fail "the Docker network $KIND_NETWORK has no IPv4 gateway"

EMU_HOME=$WORK/home
mkdir -p "$EMU_HOME"
# Bound to the gateway, and named in the certificate by EMULATOR_BIND_ADDRESS, so a pod
# verifies https://<gateway>:<port> with the emulator's CA.
# The access log (fork/access-log) records every call the controllers make. Without
# listener sockets an ALB publishes an elb.amazonaws.com name, which an Ingress status
# accepts as a hostname; a socket's host:port is refused there.
RUST_LOG=${EMULATOR_LOG:-info,access=debug} EMULATOR_ELB_LISTENER_SOCKETS=off EMULATOR_BIND_ADDRESS=$GATEWAY HOME=$EMU_HOME "$BIN" -b "$GATEWAY" -p "$PORT" --telemetry off >"$WORK/emulator.log" 2>&1 &
EMU_PID=$!
EMU_DIR=$EMU_HOME/.aws/emulator/$PORT
CA=$EMU_DIR/ca.pem
export AWS_ENDPOINT_URL=https://$GATEWAY:$PORT
wait_for 60 curl -sf --cacert "$CA" "$AWS_ENDPOINT_URL/__admin/health" || fail "the emulator did not answer at $AWS_ENDPOINT_URL; see $WORK/emulator.log"
export AWS_CA_BUNDLE=$CA AWS_SHARED_CREDENTIALS_FILE=$EMU_DIR/emulator_credentials AWS_CONFIG_FILE=$EMU_DIR/config
export AWS_REGION=$REGION AWS_DEFAULT_REGION=$REGION AWS_EC2_METADATA_DISABLED=true AWS_PAGER=
KEY_ID=$(aws configure get aws_access_key_id)
SECRET_KEY=$(aws configure get aws_secret_access_key)
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)
say "emulator at $AWS_ENDPOINT_URL, account $ACCOUNT"

# --- 2. The EKS cluster, its network and its node -----------------------------------------

VPC=$(aws ec2 create-vpc --cidr-block "$VPC_CIDR" --query Vpc.VpcId --output text)
SUBNETS=()
for spec in $NODE_SUBNET_CIDR:${REGION}a $OTHER_SUBNET_CIDR:${REGION}b; do
  SUBNETS+=("$(aws ec2 create-subnet --vpc-id "$VPC" --cidr-block "${spec%%:*}" --availability-zone "${spec##*:}" \
    --tag-specifications "ResourceType=subnet,Tags=[{Key=kubernetes.io/role/internal-elb,Value=1},{Key=kubernetes.io/cluster/$CLUSTER,Value=shared}]" \
    --query Subnet.SubnetId --output text)")
done
trust() { jq -cn --arg principal "$1" '{Version:"2012-10-17",Statement:[{Effect:"Allow",Principal:{Service:$principal},Action:["sts:AssumeRole","sts:TagSession"]}]}'; }
CLUSTER_ROLE=$(aws iam create-role --role-name "$CLUSTER-cluster" --assume-role-policy-document "$(trust eks.amazonaws.com)" --query Role.Arn --output text)
aws eks create-cluster --name "$CLUSTER" --role-arn "$CLUSTER_ROLE" --resources-vpc-config "subnetIds=$(IFS=,; echo "${SUBNETS[*]}")" >>"$LOG"
aws eks wait cluster-active --name "$CLUSTER"
ISSUER=$(aws eks describe-cluster --name "$CLUSTER" --query cluster.identity.oidc.issuer --output text)
# The node's EC2 instance, which OpenCost reads the instance type and Region of.
AMI=$(aws ssm get-parameter --name /aws/service/eks/optimized-ami/1.34/amazon-linux-2023/x86_64/standard/recommended/image_id --query Parameter.Value --output text)
say "EKS cluster $CLUSTER, issuer $ISSUER, VPC $VPC"

cat >"$WORK/kind.yaml" <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
networking:
  podSubnet: $POD_CIDR
nodes:
- role: control-plane
  labels:
    node.kubernetes.io/instance-type: $NODE_INSTANCE_TYPE
    topology.kubernetes.io/region: $REGION
    topology.kubernetes.io/zone: ${REGION}a
  kubeadmConfigPatches:
  - |
    kind: ClusterConfiguration
    apiServer:
      extraArgs:
        service-account-issuer: $ISSUER
  extraMounts:
  - hostPath: $WORK/price-list
    containerPath: /price-list
EOF
mkdir -p "$WORK/price-list"
kind delete cluster --name "$KIND_CLUSTER" >>"$LOG" 2>&1 || true
kind create cluster --name "$KIND_CLUSTER" --config "$WORK/kind.yaml" --kubeconfig "$KUBECONFIG" --wait 120s >>"$LOG" 2>&1 || fail "kind create cluster; see $LOG"

# The node's EC2 instance, at the node container's own address, which OpenCost reads the
# instance type and Region of and a load balancer dials as its target. The node's
# providerID is set once, from empty, which the API server allows.
NODE=$KIND_CLUSTER-control-plane
NODE_IP=$(docker inspect -f "{{(index .NetworkSettings.Networks \"$KIND_NETWORK\").IPAddress}}" "$NODE")
[[ -n $NODE_IP ]] || fail "the kind node has no address on $KIND_NETWORK"
INSTANCE=$(aws ec2 run-instances --image-id "$AMI" --instance-type "$NODE_INSTANCE_TYPE" --subnet-id "${SUBNETS[0]}" \
  --private-ip-address "$NODE_IP" --query 'Instances[0].InstanceId' --output text) || fail "launching the node instance at $NODE_IP"
kubectl patch node "$NODE" -p "{\"spec\":{\"providerID\":\"aws:///${REGION}a/$INSTANCE\"}}" >>"$LOG" || fail "setting the node's providerID"
say "node $NODE at $NODE_IP is instance $INSTANCE"

# kind's API server now signs as the EKS cluster's issuer: register its key set for it.
kubectl get --raw /openid/v1/jwks >"$WORK/jwks.json"
curl -sf --cacert "$CA" --aws-sigv4 "aws:amz:$REGION:eks" --user "$KEY_ID:$SECRET_KEY" -X PUT \
  -H 'content-type: application/json' --data-binary "@$WORK/jwks.json" \
  "$AWS_ENDPOINT_URL/_fork/eks/clusters/$CLUSTER/service-account-issuer-keys" >>"$LOG" || fail "registering the service account key set"
say "kind cluster $KIND_CLUSTER signs as $ISSUER"

for namespace in kube-system external-secrets opencost probes; do
  kubectl create namespace "$namespace" --dry-run=client -o yaml | kubectl apply -f - >>"$LOG"
  kubectl -n "$namespace" create configmap emulator-ca --from-file=ca.pem="$CA" >>"$LOG"
done

# --- 3. IAM: the OIDC provider, the IRSA roles and the Pod Identity association ----------

PROVIDER=${ISSUER#https://}
aws iam create-open-id-connect-provider --url "$ISSUER" --client-id-list sts.amazonaws.com \
  --thumbprint-list 9e99a48a9960b14926bb7f3b02e22da2b0ab7280 >>"$LOG"
everything='{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"*","Resource":"*"}]}'
irsa_role() {
  local name=$1 namespace=$2 account=$3 document
  document=$(jq -cn --arg provider "$PROVIDER" --arg federated "arn:aws:iam::$ACCOUNT:oidc-provider/$PROVIDER" \
    --arg subject "system:serviceaccount:$namespace:$account" \
    '{Version:"2012-10-17",Statement:[{Effect:"Allow",Principal:{Federated:$federated},Action:"sts:AssumeRoleWithWebIdentity",
      Condition:{StringEquals:{($provider+":sub"):$subject,($provider+":aud"):"sts.amazonaws.com"}}}]}')
  aws iam create-role --role-name "$name" --assume-role-policy-document "$document" --query Role.Arn --output text
  aws iam put-role-policy --role-name "$name" --policy-name everything --policy-document "$everything"
}
IRSA_PROBE_ROLE=$(irsa_role irsa-probe probes irsa-probe)
LBC_ROLE=$(irsa_role load-balancer-controller kube-system aws-load-balancer-controller)
ESO_ROLE=$(irsa_role external-secrets external-secrets external-secrets)
POD_IDENTITY_ROLE=$(aws iam create-role --role-name pod-identity-probe --assume-role-policy-document "$(trust pods.eks.amazonaws.com)" --query Role.Arn --output text)
aws iam put-role-policy --role-name pod-identity-probe --policy-name everything --policy-document "$everything"
aws eks create-pod-identity-association --cluster-name "$CLUSTER" --namespace probes --service-account pod-identity-probe --role-arn "$POD_IDENTITY_ROLE" >>"$LOG"

# --- 4. The webhook and the EKS Pod Identity Agent ----------------------------------------

# The webhook's serving certificate, signed by a CA the API server is given in caBundle.
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=webhook-ca -keyout "$WORK/webhook-ca.key" -out "$WORK/webhook-ca.crt" 2>>"$LOG"
openssl req -newkey rsa:2048 -nodes -subj /CN=pod-identity-webhook.kube-system.svc -keyout "$WORK/webhook.key" -out "$WORK/webhook.csr" 2>>"$LOG"
openssl x509 -req -in "$WORK/webhook.csr" -CA "$WORK/webhook-ca.crt" -CAkey "$WORK/webhook-ca.key" -CAcreateserial -days 2 \
  -extfile <(printf 'subjectAltName=DNS:pod-identity-webhook.kube-system.svc,DNS:pod-identity-webhook.kube-system') -out "$WORK/webhook.crt" 2>>"$LOG"
kubectl -n kube-system create secret tls pod-identity-webhook-cert --cert "$WORK/webhook.crt" --key "$WORK/webhook.key" >>"$LOG"
# What EKS hands the webhook: the service accounts that have a Pod Identity association.
aws eks list-pod-identity-associations --cluster-name "$CLUSTER" \
  | jq '{identities: [.associations[] | {namespace, serviceAccount}]}' >"$WORK/pod-identity.json"
kubectl -n kube-system create configmap pod-identity-webhook-config --from-file=pod-identity.json="$WORK/pod-identity.json" >>"$LOG"
sed -e "s|@WEBHOOK_IMAGE@|$WEBHOOK_IMAGE|g" -e "s|@REGION@|$REGION|g" -e "s|@CA_BUNDLE@|$(base64 -w0 <"$WORK/webhook-ca.crt")|g" \
  "$HERE/manifests/pod-identity-webhook.yaml" | kubectl apply -f - >>"$LOG"
kubectl -n kube-system rollout status deployment/pod-identity-webhook --timeout=180s >>"$LOG" 2>&1 || fail "the pod identity webhook did not start; see $LOG"

if [[ ! -d $WORK/eks-pod-identity-agent ]]; then
  git clone -q https://github.com/aws/eks-pod-identity-agent.git "$WORK/eks-pod-identity-agent"
fi
git -C "$WORK/eks-pod-identity-agent" checkout -q "$POD_IDENTITY_AGENT_COMMIT"
docker build -q -t eks-pod-identity-agent:fork "$WORK/eks-pod-identity-agent" >>"$LOG" 2>&1 || fail "building the EKS Pod Identity Agent image; see $LOG"
kind load docker-image eks-pod-identity-agent:fork --name "$KIND_CLUSTER" >>"$LOG" 2>&1
helm upgrade --install eks-pod-identity-agent "$WORK/eks-pod-identity-agent/charts/eks-pod-identity-agent" -n kube-system \
  --set clusterName="$CLUSTER" --set image.override=eks-pod-identity-agent:fork --set image.pullPolicy=Never \
  --set env.AWS_REGION="$REGION" --set env.AWS_ACCESS_KEY_ID="$KEY_ID" --set env.AWS_SECRET_ACCESS_KEY="$SECRET_KEY" \
  --set env.AWS_CA_BUNDLE=/etc/emulator/ca.pem --set env.SSL_CERT_FILE=/etc/emulator/ca.pem \
  --set-string "agent.additionalArgs.--endpoint=$AWS_ENDPOINT_URL" \
  -f "$HERE/values/eks-pod-identity-agent.yaml" --wait --timeout 180s >>"$LOG" 2>&1 || fail "the EKS Pod Identity Agent did not start; see $LOG"
say "webhook and EKS Pod Identity Agent running"

# --- 5. The checks ------------------------------------------------------------------------

# What a failed check leaves for diagnosis, in $WORK/diagnostics/<check>/: the cluster's
# pods and events, and the full logs of every pod in the namespaces given.
diagnose() {
  local check=$1; shift
  local dir=$WORK/diagnostics/$check
  mkdir -p "$dir"
  kubectl get pods -A -o wide >"$dir/pods.txt" 2>&1 || true
  kubectl get events -A --sort-by=.lastTimestamp >"$dir/events.txt" 2>&1 || true
  local namespace pod
  for namespace in "$@"; do
    for pod in $(kubectl -n "$namespace" get pods -o name 2>/dev/null); do
      kubectl -n "$namespace" describe "$pod" >"$dir/$namespace-${pod#pod/}.describe.txt" 2>&1 || true
      kubectl -n "$namespace" logs "$pod" --all-containers --prefix --tail=-1 >"$dir/$namespace-${pod#pod/}.log" 2>&1 || true
    done
  done
  say "$check: diagnostics in $dir"
}

# Run the AWS CLI in a pod as service account $1 and print what it answers.
probe() {
  local account=$1; shift
  kubectl -n probes delete pod "$account" --ignore-not-found >>"$LOG" 2>&1
  sed -e "s|@NAME@|$account|g" -e "s|@IMAGE@|$AWS_CLI_IMAGE|g" -e "s|@ENDPOINT@|$AWS_ENDPOINT_URL|g" -e "s|@REGION@|$REGION|g" \
    -e "s|@ARGS@|$(printf '"%s",' "$@" | sed 's/,$//')|g" "$HERE/manifests/probe.yaml" | kubectl apply -f - >>"$LOG"
  kubectl -n probes wait pod "$account" --for=jsonpath='{.status.phase}'=Succeeded --timeout=120s >>"$LOG" 2>&1 || true
  kubectl -n probes logs "$account" 2>&1
}

check_irsa() {
  kubectl -n probes create serviceaccount irsa-probe --dry-run=client -o yaml | kubectl apply -f - >>"$LOG"
  kubectl -n probes annotate serviceaccount irsa-probe --overwrite "eks.amazonaws.com/role-arn=$IRSA_PROBE_ROLE" >>"$LOG"
  local answer
  answer=$(probe irsa-probe sts get-caller-identity --query Arn --output text)
  say "irsa: $answer"
  [[ $answer == "arn:aws:sts::$ACCOUNT:assumed-role/irsa-probe/"* ]] || { diagnose irsa probes kube-system; return 1; }
}

check_pod_identity() {
  kubectl -n probes create serviceaccount pod-identity-probe --dry-run=client -o yaml | kubectl apply -f - >>"$LOG"
  local answer
  answer=$(probe pod-identity-probe sts get-caller-identity --query Arn --output text)
  say "pod-identity: $answer"
  [[ $answer == "arn:aws:sts::$ACCOUNT:assumed-role/pod-identity-probe/"* ]] || { diagnose pod-identity probes kube-system; return 1; }
}

check_load_balancer() {
  helm upgrade --install aws-load-balancer-controller eks/aws-load-balancer-controller --version "$LBC_CHART_VERSION" -n kube-system \
    --set clusterName="$CLUSTER" --set region="$REGION" --set vpcId="$VPC" \
    --set serviceAccount.annotations."eks\.amazonaws\.com/role-arn"="$LBC_ROLE" \
    --set "env.AWS_ENDPOINT_URL=$AWS_ENDPOINT_URL" --set "env.AWS_ENDPOINT_URL_STS=$AWS_ENDPOINT_URL" \
    -f "$HERE/values/aws-load-balancer-controller.yaml" --wait --timeout 300s >>"$LOG" 2>&1 || { say "load-balancer: the controller did not start"; diagnose load-balancer kube-system probes; return 1; }
  kubectl apply -f "$HERE/manifests/ingress-probe.yaml" >>"$LOG"
  local hostname=
  for _ in $(seq 180); do
    hostname=$(kubectl -n probes get ingress echo -o jsonpath='{.status.loadBalancer.ingress[0].hostname}' 2>/dev/null || true)
    [[ -n $hostname ]] && break
    sleep 1
  done
  say "load-balancer: ingress hostname ${hostname:-<none>}"
  [[ -n $hostname ]] || { diagnose load-balancer kube-system probes; return 1; }
  local balancer group pod_ip targets
  balancer=$(aws elbv2 describe-load-balancers --query "LoadBalancers[?DNSName=='$hostname'].LoadBalancerArn | [0]" --output text)
  [[ $balancer == arn:* ]] || { say "load-balancer: no ALB in the emulator has DNS name $hostname"; diagnose load-balancer kube-system probes; return 1; }
  group=$(aws elbv2 describe-target-groups --load-balancer-arn "$balancer" --query 'TargetGroups[0].TargetGroupArn' --output text)
  # Instance targets: the kind node is the EC2 instance its providerID names, and the
  # Service's NodePort is the port the target group forwards to.
  for _ in $(seq 60); do
    targets=$(aws elbv2 describe-target-health --target-group-arn "$group" --query 'TargetHealthDescriptions[].Target.Id' --output text)
    [[ " $targets " == *" $INSTANCE "* ]] && break
    sleep 1
  done
  say "load-balancer: $balancer targets [$targets], node instance $INSTANCE"
  [[ " $targets " == *" $INSTANCE "* ]] || { diagnose load-balancer kube-system probes; return 1; }
  local health=
  for _ in $(seq 120); do
    health=$(aws elbv2 describe-target-health --target-group-arn "$group" --query 'TargetHealthDescriptions[0].TargetHealth.State' --output text)
    [[ $health == healthy ]] && break
    sleep 1
  done
  say "load-balancer: target health $health"
  [[ $health == healthy ]] || { diagnose load-balancer kube-system probes; return 1; }
  # Real traffic: a request to the ALB's name, at the emulator's endpoint, is passed on to
  # the node's NodePort and answered by the echo pod's nginx.
  local page=
  for _ in $(seq 30); do
    page=$(curl -s --max-time 10 --cacert "$CA" -H "Host: $hostname" "$AWS_ENDPOINT_URL/" || true)
    [[ $page == *"Welcome to nginx!"* ]] && break
    sleep 2
  done
  say "load-balancer: GET http://$hostname/ answered: $(printf '%s' "$page" | tr -s '[:space:]' ' ' | cut -c1-120)"
  [[ $page == *"Welcome to nginx!"* ]] || { diagnose load-balancer kube-system probes; return 1; }
}

check_external_secrets() {
  local value="probe-$RANDOM-$RANDOM"
  aws secretsmanager create-secret --name probes/external-secret --secret-string "$value" >>"$LOG"
  helm upgrade --install external-secrets external-secrets/external-secrets --version "$ESO_CHART_VERSION" -n external-secrets \
    --set serviceAccount.annotations."eks\.amazonaws\.com/role-arn"="$ESO_ROLE" \
    -f <(sed "s|@ENDPOINT@|$AWS_ENDPOINT_URL|g" "$HERE/values/external-secrets.yaml") --wait --timeout 300s >>"$LOG" 2>&1 || { say "external-secrets: the operator did not start"; diagnose external-secrets external-secrets kube-system; return 1; }
  sed -e "s|@REGION@|$REGION|g" "$HERE/manifests/external-secret.yaml" | kubectl apply -f - >>"$LOG" 2>&1 \
    || { sleep 20; sed -e "s|@REGION@|$REGION|g" "$HERE/manifests/external-secret.yaml" | kubectl apply -f - >>"$LOG"; }
  local copied=
  for _ in $(seq 120); do
    copied=$(kubectl -n probes get secret external-secret -o jsonpath='{.data.value}' 2>/dev/null | base64 -d 2>/dev/null || true)
    [[ -n $copied ]] && break
    sleep 1
  done
  say "external-secrets: secret holds '${copied:-<nothing>}', Secrets Manager holds '$value'"
  [[ $copied == "$value" ]] || { kubectl -n probes describe externalsecret external-secret >>"$LOG" 2>&1; diagnose external-secrets external-secrets kube-system; return 1; }
}

check_opencost() {
  # The emulator serves price list files on a loopback listener a pod cannot reach, so the
  # EC2 file for the Region is fetched here and served in the cluster from the node mount.
  local url
  url=$(aws pricing get-price-list-file-url --region us-east-1 --file-format json \
    --price-list-arn "$(aws pricing list-price-lists --region us-east-1 --service-code AmazonEC2 --currency-code USD \
      --effective-date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --region-code "$REGION" --query 'PriceLists[0].PriceListArn' --output text)" \
    --query Url --output text)
  curl -sf --cacert "$CA" "$url" -o "$WORK/price-list/index.json" || curl -sf "$url" -o "$WORK/price-list/index.json" || { say "opencost: could not fetch $url"; return 1; }
  kubectl apply -f "$HERE/manifests/price-list-server.yaml" >>"$LOG"
  kubectl -n opencost rollout status deployment/price-list --timeout=120s >>"$LOG" 2>&1
  helm upgrade --install opencost opencost/opencost --version "$OPENCOST_CHART_VERSION" -n opencost \
    -f "$HERE/values/opencost.yaml" --wait --timeout 300s >>"$LOG" 2>&1 || { say "opencost: did not start"; diagnose opencost opencost; return 1; }
  local expected
  expected=$(aws pricing get-products --region us-east-1 --service-code AmazonEC2 --filters \
    Type=TERM_MATCH,Field=instanceType,Value="$NODE_INSTANCE_TYPE" Type=TERM_MATCH,Field=regionCode,Value="$REGION" \
    Type=TERM_MATCH,Field=operatingSystem,Value=Linux Type=TERM_MATCH,Field=tenancy,Value=Shared \
    Type=TERM_MATCH,Field=preInstalledSw,Value=NA Type=TERM_MATCH,Field=capacitystatus,Value=Used \
    --query 'PriceList[0]' --output text | jq -r '.terms.OnDemand[].priceDimensions[].pricePerUnit.USD')
  # OpenCost reports default CPU and memory prices until it has read the price list, so
  # wait for the reported cost to become the emulator's price.
  local reported=
  for _ in $(seq 60); do
    reported=$(kubectl get --raw /api/v1/namespaces/opencost/services/opencost:9003/proxy/metrics 2>/dev/null \
      | awk '/^node_total_hourly_cost\{/ {print $NF; exit}' || true)
    same_price "$reported" "$expected" && break
    sleep 5
  done
  say "opencost: node_total_hourly_cost ${reported:-<none>}, emulator price for $NODE_INSTANCE_TYPE $expected"
  same_price "$reported" "$expected" || { diagnose opencost opencost; return 1; }
}

# Whether two hourly prices agree to a hundredth of a cent.
same_price() {
  [[ -n $1 && -n $2 ]] && awk -v a="$1" -v b="$2" 'BEGIN { d = a - b; if (d < 0) d = -d; exit !(d < 0.0001) }'
}

failed=0
printf '%-18s %s\n' CHECK RESULT
for check in "${CHECKS[@]}"; do
  if "check_${check//-/_}"; then result=ok; else result=FAIL; failed=1; fi
  printf '%-18s %s\n' "$check" "$result"
done
say "log: $LOG, emulator log: $WORK/emulator.log"
exit "$failed"
