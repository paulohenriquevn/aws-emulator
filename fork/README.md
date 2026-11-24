# Fork layer

This fork of [aws/aws-emulator](https://github.com/aws/aws-emulator) runs Terraform stacks built
on the community AWS modules (VPC, EKS, RDS, ElastiCache, CloudFront and more) without an AWS
account. Everything the fork adds lives in this directory, plus a few patches inside upstream crates, each marked with a `fork:` comment.

## What is here

| Path | What |
|---|---|
| `services.txt` | The 29 AWS service crates the router mounts. The upstream router builds every service and needs about 100 GB of build output; ours builds only these. |
| `render_router.py` | Generates `router/` from `services.txt`. `--check` fails when the generated files are stale. |
| `router/` | The slim router crate. Its binary is `aws-emulator-fork`. |
| `console/` | The read-only web console the binary serves at `/_console/`: a React app in `console/web` and the crate that embeds its build. |
| `acceptance/` | Eight Terraform fixtures (billing, compute, data, edge, network, operations, registry, security) and `run.sh`, which runs each one against a fresh emulator. |
| `service-account-issuers/` | The route that registers a Kubernetes cluster's service account key set for an EKS cluster of the emulator. |
| `eks-provisioner/` | A Go program that runs, for every EKS cluster of the emulator, its control plane and its node group instances in kind, and answers the API servers' token reviews from the cluster's access entries (see EKS clusters through the API). |
| `eks-control-plane/` | The route a provisioner outside the emulator reads an EKS cluster's issuer and CA from, and registers the API server it brought up with (see External control planes). |
| `iam-gate/` | Judges every signed call against the caller's IAM policies when `EMULATOR_IAM_ENFORCE=on` (see IAM on every service). |
| `access-log/` | Logs one line per request the binary answers (`RUST_LOG=info,access=debug`): the operation, its status and error code, and how long it took. |
| `kubernetes/` | `run.sh`, which runs real in-cluster controllers and workload identity in a kind cluster against the emulator, with the manifests and chart values it applies. |

## Usage

```bash
python3 fork/render_router.py            # after editing services.txt
cargo build -p aws-emulator-fork-router
fork/acceptance/run.sh target/debug/aws-emulator-fork           # every fixture
fork/acceptance/run.sh target/debug/aws-emulator-fork network   # one fixture
```

A fixture passes when `apply` succeeds, a second `plan` shows no changes, and `destroy` leaves
an empty state. A fixture with a `check.sh` runs it after `apply`, against the same emulator, to
exercise calls a controller makes at run time and Terraform never does (a CloudFront invalidation,
an EventBridge event reaching its SQS target). `run.sh` needs Terraform 1.10 or later and network access to download the AWS
provider.

The EC2 crate needs several GB of memory to compile. The workflow
`.github/workflows/fork-acceptance.yml` runs the regression tests, the fixtures and the full suites
of the patched services on every pull request.

## Console

The binary serves a read-only console at `https://localhost:9297/_console/`. It lists what the
emulator holds in 23 services (EC2, VPC, S3, RDS, EKS, IAM and the rest of `services.txt` that has
something to list), with a detail page per resource. The page calls the emulator's own AWS APIs
from the browser with the AWS SDK for JavaScript, so it shows what any other client would read.

Sign in with the keys in `~/.aws/emulator/9297/emulator_credentials` (any pair works with
`--auth off`). The browser warns about the certificate because it comes from the emulator's own
CA; accept it, or trust `~/.aws/emulator/9297/ca.pem`. The keys stay in the tab's session storage.
The console never reads a secret or parameter value, only the metadata the list calls return.

```bash
cd fork/console/web
npm ci && npm run build      # before cargo build, or /_console/ answers 503
npm test                     # unit tests
EMULATOR_BIN=$PWD/../../../target/debug/aws-emulator-fork npm run e2e   # against a real emulator
EMULATOR_URL=https://localhost:9297 npm run dev   # dev server; API calls go to the emulator
node --experimental-strip-types e2e/seed.ts https://localhost:9297 ~/.aws/emulator/9297   # one of everything
```

The end-to-end suite seeds a fresh emulator with one resource of every type the console lists
(`e2e/seed.ts`), then checks that each list page shows it, that its detail page opens with its
child resources, and that no page shows a secret or parameter value. A resource type added to
`src/catalog.ts` without a seed fails the suite.

`cargo build` embeds `console/web/dist` in the binary. Without it the build still succeeds and
prints a warning; set `AWS_EMULATOR_REQUIRE_CONSOLE=1` to make that an error, as CI does.

## Kubernetes

The emulator does not run Kubernetes. A kind cluster stands in for an EKS cluster's control
plane: the EKS cluster is created in the emulator, kind's API server signs service account tokens
as that cluster's OIDC issuer, and kind's public key set is registered for the issuer:

```bash
kubectl get --raw /openid/v1/jwks > jwks.json
curl --cacert "$AWS_CA_BUNDLE" --aws-sigv4 aws:amz:us-east-1:eks --user "$KEY:$SECRET" \
  -X PUT --data-binary @jwks.json \
  "$AWS_ENDPOINT_URL/_fork/eks/clusters/<cluster>/service-account-issuer-keys"
```

From then on `sts:AssumeRoleWithWebIdentity` (IRSA) and `eks-auth:AssumeRoleForPodIdentity`
(EKS Pod Identity) accept that cluster's tokens with the checks AWS makes: signature, issuer,
audience, lifetime, and the role's trust policy or the cluster's pod identity association. With
authentication on, the route takes only requests signed with the emulator's own keys. It answers
404 for a cluster the emulator does not hold in the request's Region.

`fork/kubernetes/run.sh <emulator-binary>` builds all of it and checks that it works:

| Check | Passes when |
|---|---|
| `irsa` | A pod whose service account is annotated with a role is that role (`sts get-caller-identity`). |
| `iam-enforced` | The IRSA probe's role allows only `sts:GetCallerIdentity`, and EC2 refuses its `DescribeVpcs` with `UnauthorizedOperation`. |
| `pod-identity` | A pod whose service account has a pod identity association gets the role's credentials from the EKS Pod Identity Agent. |
| `node-group` | The EKS managed node group's instances are the kind workers: `kubectl get nodes` lists one worker per instance, each `providerID` naming its instance, and the workers are `Ready`. |
| `load-balancer` | The AWS Load Balancer Controller, on IRSA, turns an Ingress into an ALB in the emulator with every node group instance registered as a target (instance targets: a kind pod has no VPC network interface for an IP target), every target healthy, and a request to the ALB's name answered by the echo pod's nginx. |
| `database` | A pod runs `select 1` with `psql` over TLS on an RDS for PostgreSQL instance (a real PostgreSQL container) whose security group admits the nodes' groups on its port, and gets `1`; on an instance whose group has no inbound rule, the connection is closed. |
| `external-secrets` | External Secrets, on IRSA, copies a Secrets Manager secret into a Kubernetes Secret. |
| `opencost` | OpenCost reports the node's hourly cost as the emulator's on-demand price for its instance type. |

A controller built on the AWS SDK for Go v2 that gets IRSA credentials from the default credential
chain needs `AWS_ENDPOINT_URL_STS` as well as `AWS_ENDPOINT_URL`. The SDK builds that chain's STS
client before it reads `AWS_ENDPOINT_URL` (`aws-sdk-go-v2/config`, `resolveCredentials` runs
before `resolveBaseEndpoint`), and the client then reads only the STS-specific variable, so
without it the token goes to `sts.<region>.amazonaws.com`. A component that builds its own HTTP
client also ignores `AWS_CA_BUNDLE` (the EKS Pod Identity Agent does); `SSL_CERT_FILE` pointing at
the emulator's CA covers both cases.

The harness runs the emulator with `EMULATOR_ELB_LISTENER_SOCKETS=off`. By default an
Application Load Balancer gets a socket of its own and publishes its `host:port` as `DNSName`,
which the Kubernetes API refuses as the hostname of an Ingress status, so the controller could
never report the load balancer. Without sockets the load balancer publishes
`<name>-<id>.<region>.elb.amazonaws.com` and is served at the emulator's endpoint for a request
whose `Host` is that name.

The ALB passes that request on to its target (`target_proxy.rs`, below). For the target to be
reachable, the kind node runs on a Docker network of its own (`aws-emulator-kind`,
`172.29.0.0/16`, override with `KIND_NETWORK` and `KIND_NETWORK_CIDR`) that is also the EKS
cluster's VPC. The node's EC2 instance is launched at `172.29.100.10` before kind starts, so
kubelet registers the node with that instance as its `providerID` (the API server lets a
`providerID` be set only from empty, and kind sets one of its own otherwise). That address is then
added to the node container's interface. Docker hands out addresses of `172.29.1.0/24` only, so
it never gives the instance's address to a container.

The harness also creates a managed node group of two `t3.medium` nodes. `CreateNodegroup`
launches the instances in EC2 (below), and each one becomes a kind worker the same way: its
`providerID`, zone and `eks.amazonaws.com/nodegroup` label in `kind.yaml`, and its private address
added to the worker container's interface. The control-plane node keeps the instance launched
above.

Each node also sends its traffic to the emulator from the instance's address: its own through
a route (`ip route add <gateway>/32 src <instance address>`), its pods' TCP through a SNAT rule
ahead of kind's masquerade, which would otherwise pick the node's Docker address. A connection
from the node or a pod then arrives from the address an ENI of the account holds, as from an
EKS node. The security groups of an
RDS or ElastiCache endpoint then judge the node instance's groups (see "Security groups on
database endpoints"). The harness runs RDS with `EMULATOR_RDS_BACKEND=docker`.

With `KEEP_CLUSTER=1` the emulator and the cluster stay up after the checks, and
`$KUBERNETES_WORK/environment.sh` holds what a consumer needs to keep working on them: the AWS
CLI's endpoint, CA and credentials, `KUBECONFIG`, the EKS cluster's name and issuer, the VPC,
its subnets and the node instances' security groups. Name the checks a consumer needs, such
as `run.sh <binary> load-balancer`, to install only what they install.

It needs docker, kind, kubectl, helm, the AWS CLI v2, curl, jq, openssl and git, and network
access for images and charts. The emulator listens on the gateway of kind's Docker network
(`-b`), and every pod that calls it mounts its CA. The `kubernetes` job of the workflow runs it.

### External control planes

With `EMULATOR_EKS_CONTROL_PLANE=external`, the emulator leaves an EKS cluster's control plane to
a provisioner outside it, as Amazon EKS provisions one behind its API. `CreateCluster` answers
and the cluster stays `CREATING` until that provisioner registers the API server it brought up;
`DescribeCluster` reports no `endpoint`, `identity` or certificate data until then, which is what
tells a client (Terraform's `aws_eks_cluster`, `aws eks wait cluster-active`) to keep waiting.
Unset or `emulated`, the clock activates the cluster as before. Any other value fails
`CreateCluster` with a `ServerException` that names it.

The provisioner reads what it needs while the cluster is `CREATING`:

```bash
curl --cacert "$AWS_CA_BUNDLE" --aws-sigv4 aws:amz:us-east-1:eks --user "$KEY:$SECRET" \
  "$AWS_ENDPOINT_URL/_fork/eks/clusters/<cluster>/control-plane"
# {"name", "status", "version", "issuer", "endpoint",
#  "certificateAuthority": {"id", "data", "signingKey"}}
```

`certificateAuthority` is the CA Amazon EKS issued the cluster: `data` is the Base64 PEM
certificate `DescribeCluster` reports, and `signingKey` its PEM private key. The API server's
certificates are signed with it (kubeadm uses an existing `ca.crt` and `ca.key`), so a
kubeconfig built from `DescribeCluster` trusts the real API server. No AWS operation reports the
key; only this route does, to requests signed with the emulator's own keys.

Then it registers the API server:

```bash
curl --cacert "$AWS_CA_BUNDLE" --aws-sigv4 aws:amz:us-east-1:eks --user "$KEY:$SECRET" \
  -X PUT --data '{"endpoint": "https://172.29.1.20:6443"}' \
  "$AWS_ENDPOINT_URL/_fork/eks/clusters/<cluster>/control-plane"
```

The cluster turns `ACTIVE` and reports that endpoint. The endpoint must be `https://host[:port]`
with no path (400 otherwise); a connected cluster is refused with 400, a `DELETING` one with 409,
and an unknown one with 404. Registering again updates the endpoint, and works the same on a
cluster the clock activated.

Not modelled: a successor CA activated with `ActivateCertificateAuthority` is not rolled into a
running control plane. The route hands out the CA that is signing, so a control plane rebuilt
after the activation uses it.

### EKS clusters through the API

`fork/eks-provisioner` turns the EKS API into working Kubernetes clusters, so Terraform's
`aws_eks_cluster` and `aws_eks_node_group` (or any client of the API) get clusters that a
kubeconfig from `aws eks update-kubeconfig` reaches. Run the emulator with
`EMULATOR_EKS_CONTROL_PLANE=external` and the provisioner beside it, with the same AWS
environment a client of the emulator uses:

```bash
AWS_ENDPOINT_URL=https://172.29.0.1:9597 AWS_CA_BUNDLE=... AWS_SHARED_CREDENTIALS_FILE=.../emulator_credentials \
  eks-provisioner --regions us-east-1 --state-dir /var/lib/aws-emulator-eks --listen :9598
```

Every few seconds it reconciles each Region:

| EKS | What the provisioner does |
|---|---|
| A cluster is `CREATING` | Creates a network interface in the cluster's first subnet (`Amazon EKS <cluster>`), and a kind control plane on the VPC's Docker network that signs with the cluster's CA, issues service account tokens as its OIDC issuer and answers at the interface's address on port 6443. Registers the service account key set and the endpoint, which turns the cluster `ACTIVE`. |
| A node group instance is `running` | Runs a kind node container at the instance's private address, named after its private DNS name, and joins it with the instance's providerID, the node group's labels and taints, and the `eks.amazonaws.com/nodegroup` label. |
| An instance is gone | Deletes its Node and its container. |
| The cluster is gone | Deletes the kind cluster and the network interface. |

The VPC's Docker network has the VPC's CIDR and hands out container addresses only from the
highest /27 no subnet covers, so it never hands out an address EC2 assigns. A network that
already has the VPC's CIDR is used as it is; a new one is named
`aws-emulator-<vpc-id>-<cidr>`, since two emulators on one host hand out the same VPC ids.

The API servers authenticate a bearer token through the provisioner (`--listen`, which they
dial at the gateway of their VPC's network): the token's presigned `GetCallerIdentity` is sent
to the emulator's STS with the cluster's name, and the caller's principal (an assumed-role
session is its role) must have an access entry on the cluster. Its username and Kubernetes
groups are the entry's, and each associated access policy binds its ClusterRole:
`AmazonEKSClusterAdminPolicy` is `cluster-admin`, `AmazonEKSAdminPolicy` `admin`,
`AmazonEKSEditPolicy` `edit` and `AmazonEKSViewPolicy` `view`, cluster-wide or in the
namespaces of its scope. The principal that created the cluster has the cluster-admin entry
`CreateCluster` gives it.

A token is accepted only when its request goes to the emulator: the host of
`AWS_ENDPOINT_URL`, or a name in `--sts-aliases` (on the emulator's own host, clients reach it
at `localhost`, and S3 Control needs that name). Whatever the name, the provisioner sends the
request to `AWS_ENDPOINT_URL` with that name as its `Host`, which the signature covers.

Before it registers a cluster, and again every minute while the cluster runs, the provisioner
installs what Amazon EKS runs on its control plane, so a chart written for Amazon EKS needs no
change here:

| Add-on | What a pod gets |
|---|---|
| `amazon-eks-pod-identity-webhook` v0.6.19, pinned by digest, in `kube-system` on the control-plane node | IRSA, as on Amazon EKS: a pod whose service account carries `eks.amazonaws.com/role-arn` gets `AWS_ROLE_ARN`, `AWS_WEB_IDENTITY_TOKEN_FILE`, a projected token for the audience `sts.amazonaws.com`, `AWS_REGION`, `AWS_DEFAULT_REGION` and `AWS_STS_REGIONAL_ENDPOINTS=regional`. |
| The MutatingWebhookConfiguration `aws-emulator-endpoint`, served by the provisioner at `/mutate/{region}/{cluster}` | Every container and init container gets `AWS_ENDPOINT_URL` and `AWS_ENDPOINT_URL_STS` (the provisioner's own `AWS_ENDPOINT_URL`), `AWS_CA_BUNDLE` and `SSL_CERT_FILE` (the public roots plus the emulator's CA), and `NODE_EXTRA_CA_CERTS` (the emulator's CA), from a ConfigMap mounted read-only at `/etc/aws-emulator`. |

The pod identity webhook runs on the control-plane node, where Amazon EKS runs it, so it serves
pods created before any node has joined, and the cluster turns `ACTIVE` only once it is
available. Its serving certificate is signed by the provisioner's `webhook-ca.crt`, which the
API server trusts already, and kept in the cluster's state directory.

Its image comes from ECR Public. A host whose address ECR Public throttles (GitHub's shared
runners get `429 Data limit exceeded`) runs a copy instead, with
`--pod-identity-webhook-image` or `POD_IDENTITY_WEBHOOK_IMAGE`; the provisioner refuses a
reference that is not pinned by digest. The fork's CI copies the image to GHCR under the same
digest and runs that.

The endpoint webhook leaves alone a pod with a container that sets `AWS_ENDPOINT_URL`, and a
variable a container sets itself keeps its value. Its `failurePolicy` is `Fail`: while the
provisioner is down a pod outside `kube-system` is refused, and its controller creates it
again, rather than started with no endpoint and sent to `*.amazonaws.com`. In `kube-system`
only a pod with an IRSA role gets the endpoint (the AWS Load Balancer Controller, for
instance), through a second webhook whose `failurePolicy` is `Ignore`, so the cluster's own
components keep starting while the provisioner is down; it is called again after the pod
identity webhook gives a pod its role. The ConfigMap is created in the pod's namespace while the pod is admitted
(not on a dry run), and named after its content (`aws-emulator-ca-<hash>`, immutable), so a
namespace created a moment earlier already has it, and a changed CA is a new ConfigMap that
new pods mount. The public roots are the host's bundle (`--system-roots`, by default
`SSL_CERT_FILE` or the distribution's file); a pod that trusts roots of its own besides those
sets `SSL_CERT_FILE` itself.

Nodes pull from the emulator's ECR the way an Amazon EKS node pulls from ECR, with no
`imagePullSecret` in the pod. The registry host is the host of `AWS_ENDPOINT_URL`
(`172.29.0.1:9597` on the staging host), so a pod names its image
`<that host>/<repository>@sha256:...`, not the `repositoryUri` ECR reports (`localhost:9597/...`,
which on a node is the node itself). On every node it joins, and once per run on every node
that joined before, the provisioner writes:

| File on the node | What it does |
|---|---|
| `/etc/containerd/certs.d/<host>/hosts.toml` and `ca.crt` | containerd trusts the emulator's CA for the registry host. containerd 2 in kind's node images reads this directory by default. |
| `/etc/eks/image-credential-provider/config.yaml` | The kubelet's `CredentialProviderConfig`: the provider `ecr-credential-provider` for images of the registry host, cached 10 minutes. |
| `/etc/eks/image-credential-provider/ecr-credential-provider` | The provider, a script that sends the kubelet's `CredentialProviderRequest` to the provisioner at `/registry-credentials/{region}/{cluster}`, on the gateway of the node's network and the `--listen` port, with the kubelet's client certificate. |

The kubelet starts with `--image-credential-provider-config` and
`--image-credential-provider-bin-dir` from the node's JoinConfiguration; a node that joined
without them gets them in `kubeadm-flags.env`, and its kubelet is restarted once. The
provisioner answers only a client certificate the cluster's CA signed for a `system:node:`
user in `system:nodes`, so a pod cannot ask. Each answer carries a token ECR
`GetAuthorizationToken` has just issued to the provisioner's principal, as the user `AWS` and
its password. The kubelet keeps it for 10 minutes, or less when the token expires sooner, and
asks again after that, so an expired token is never used, and a token the emulator forgot
across a restart stops being used within 10 minutes.

Pods reach the emulator at `AWS_ENDPOINT_URL` through their node and the VPC's gateway, so it
must be an address the VPC networks route to (on the staging host, `172.29.0.1`, with the
firewall admitting `10.0.0.0/8`).

`fork/eks-provisioner/e2e.sh <emulator> <provisioner>` creates a cluster and a node group with
the AWS CLI, waits for them to turn `ACTIVE`, lists the instances as Ready nodes with a
kubeconfig from `aws eks update-kubeconfig`, runs `aws sts get-caller-identity` in a pod whose
service account names an IRSA role and that sets no AWS variable, copies an image into the
emulator's ECR with crane and runs it in a pod with no `imagePullSecret`, checks a role with no access
entry is refused, and deletes everything. A provisioner tears down only the kind clusters it
keeps a state directory for, so the script runs beside another emulator and provisioner on the
same host with `E2E_PORT` and `E2E_WEBHOOK_PORT` set to free ports.

Limits:

- Kubernetes 1.31 to 1.35, the versions kind v0.31.0 has node images for. A cluster of
  another version stays `CREATING` and the provisioner logs the versions it supports.
- A cluster name must be a valid kind cluster name: lowercase letters, digits, `-` and `.`.
- Nodes join with a kubeadm bootstrap token, not as the node role's access entry, and the
  control plane is a Node of the cluster too, tainted `NoSchedule`.
- The pod identity webhook gets no EKS Pod Identity associations, and no EKS Pod Identity
  Agent runs: a pod gets IRSA, not Pod Identity.
- A node pulls with a token issued to the provisioner's principal, not to the node role, so
  the node role's ECR permissions are not what decides the pull.
- A variable a pod takes from `envFrom` is not seen: a pod that sets `AWS_ENDPOINT_URL` only
  through a ConfigMap or a Secret gets the injected one, which takes precedence.
- Pods take addresses from `100.64.0.0/16`, not from the VPC: kind's CNI is not the Amazon
  VPC CNI. A pod's traffic leaves its node from the instance's address.
- Clusters are created one after another; each takes about a minute.

## IAM on every service

Upstream asks IAM only from IAM itself, Secrets Manager, ACM and part of KMS; every other service
serves any signed call. With `EMULATOR_IAM_ENFORCE=on`, the router asks the caller's
identity-based policies (and permissions boundary) about every call, after request authentication
and before the operation runs (`iam-gate/`). The action is `<IAM prefix>:<operation>`: the signing
name, with `monitoring` read as `cloudwatch`, and S3 operations mapped to the action IAM checks
(`ListObjectsV2` and `HeadBucket` to `s3:ListBucket`, `HeadObject` to `s3:GetObject`). A refusal
is a 403 in the service's protocol: `UnauthorizedOperation` for EC2, `AccessDeniedException` for
the JSON protocols and `AccessDenied` for the others.

The decision is simulated at the action level, because the router does not know which resource a
request addresses or what its condition keys would hold. An `Allow` counts for every action it
names, whatever its `Resource` and `Condition`. A `Deny` counts only on `"Resource": "*"` without
a condition. A role missing an action is refused, and a role whose grant is scoped by ARN or tag
(as the AWS Load Balancer Controller's policy is) is not refused by mistake. A scoped `Deny` is
not applied at all.

Not asked about: the account's own credential (the emulator's administrator), an unsigned
request, a call one emulated service makes into another, a principal holding an AWS managed
policy the emulator does not carry, and IAM, STS, Secrets Manager and ACM, which authorize their
own calls. `fork/kubernetes/run.sh` turns the gate on and gives each role the policy its
controller documents.

## Real PostgreSQL for RDS

With `EMULATOR_RDS_BACKEND=docker`, `CreateDBInstance` for engine `postgres` starts a
`postgres:<major version>` container (`EngineVersion=16.6` runs `postgres:16`; no version runs
`postgres:17`) with the master user, its password (the request's, or the one RDS generated for
the managed secret) and `DBName`. `EMULATOR_RDS_POSTGRES_IMAGE` replaces the `postgres`
repository, for a mirror. The emulator needs `docker` on its `PATH` and access to the Docker
daemon. `fork/acceptance/run.sh` turns the backend on.

The published `Endpoint` is still the emulator's own listener, so the configured `Port`, the
instance status and TLS stay the emulator's. The listener answers `SSLRequest` with the
emulator's certificate, which the emulator CA bundle verifies. It refuses a session while the
instance is not `available`, and a plaintext session while `rds.force_ssl` is on (the default
from PostgreSQL 15), with PostgreSQL's own errors. It then passes the session on to the
container, which listens on loopback only. A reboot or a stop closes the open sessions,
`ModifyDBInstance` with `MasterUserPassword` (or a rotation of the managed secret) changes the
password in the server, and `DeleteDBInstance` removes the container. Containers carry the label
`aws-emulator.service=rds`, so `docker rm -f $(docker ps -aq --filter label=aws-emulator.service=rds)`
removes the ones an emulator that was killed left behind.

Read replicas, instances restored from a snapshot, members of a DB cluster and MySQL instances
still use the in-process engine. Data lives in the container and is lost with it: a snapshot does
not copy it. A session waits up to 30 seconds for a container that is still starting (the first
start pulls the image) and is then refused as a server that is starting up.

Upstream's engine contract (`vendor/engine/src/lib.rs`) says a generated emulator runs no
containers or external processes and connects to no other host. This backend breaks that on
purpose, and only when the variable is set. The ALB target proxy makes the same kind of
exception: it connects to the registered targets.

## Real Valkey and Redis OSS for ElastiCache

With `EMULATOR_ELASTICACHE_BACKEND=docker`, a replication group with cluster mode disabled and a
standalone cache cluster of engine `valkey` or `redis` start a container of that engine and major
version (`EngineVersion=8.0` runs `valkey/valkey:8`; Redis OSS runs `redis:<major>`). `AuthToken`
becomes the server's `requirepass`, so a command sent before `AUTH` gets the server's own
`NOAUTH` error. The emulator needs `docker` on its `PATH`. `fork/acceptance/run.sh` turns the
backend on, and the `data` fixture writes and reads a value over TLS.

The group's primary and reader endpoints, and the endpoint of every node, become an emulator
listener on the emulator's host, which passes each connection on to the container (loopback
only). The listener takes the cache's `Port` (6379 by default) when it is free, on another
loopback address when 127.0.0.1 already serves it, so the Terraform AWS provider reads back the
port it set. With `TransitEncryptionEnabled` the listener runs TLS with the emulator's certificate from
the first byte. Deleting the group or the cluster removes the container; containers carry the
label `aws-emulator.service=elasticache`.

One container serves a whole group, so the reader endpoint reads from the primary and there is
no replication or failover between nodes. Cluster mode enabled groups, serverless caches and
Memcached keep the metadata-only behavior. The emulator closes a connection idle for 300
seconds, which a client's connection pool has to reconnect from.

## Security groups on database endpoints

A container-backed RDS or ElastiCache endpoint applies the DB instance's or the cache's VPC
security groups to each connection, by its source address. A replication group's groups are its
member clusters', read when the connection arrives. When a network interface of the account
holds that address, its security groups and the address are the source the inbound rules are
read against, on the instance's or the cache's port; a connection no rule admits is closed
before anything is read from it. A source no network interface holds, such as a client on the
emulator's host, is let through, and so is a connection to an instance or cache whose groups EC2
does not hold. The core passes the source address to native listeners through
`emulator_server::connection_peer()`.

## Known limits

- The EKS Pod Identity webhook reads the associations once, when `run.sh` starts it. An
  association created later reaches EKS Auth but not the webhook's configuration; restart the
  webhook with a new `pod-identity.json`, as EKS does when its own configuration changes.
- The Price List API serves its files on a loopback listener, which a pod cannot reach. `run.sh`
  fetches the EC2 file on the host and serves it in the cluster for OpenCost.

## Patches to upstream crates

Each patch starts from a failing regression test, is reported upstream, and is dropped once
upstream ships the fix.

| Service | Gap | Upstream |
|---|---|---|
| EC2 | Tags on network ACLs | aws/aws-emulator#76 |
| EC2 | Revoking an all-protocols rule that names a port range | aws/aws-emulator#46 |
| EC2 | `DescribeTags` is implemented over the resources `CreateTags` tags (`src/ops/describe_tags.rs`, `src/state/tag_descriptions.rs`) | not reported yet |
| ACM | One DNS validation CNAME for a wildcard and its apex | aws/aws-emulator#536 |
| SSM | EKS optimized AL2023 AMI parameters | aws/aws-emulator#535 |
| TLS | The certificate covers `000000000000.localhost`, the host S3 Control dials | aws/aws-emulator#56 |
| STS | A presigned `GetCallerIdentity`, an EKS token's request, is valid for 15 minutes from `X-Amz-Date` whatever its `X-Amz-Expires`, as STS accepts it (`vendor/server/src/request_authentication.rs`) | not reported yet |
| RDS (and core listeners) | An instance created with a `Port` listens on it, on another loopback address when 127.0.0.1 already serves it, so `Endpoint.Port` matches it | aws/aws-emulator#537 |
| Core server | A request signed with SigV4A (`AWS4-ECDSA-P256-SHA256`) is verified against the public key of its credential, and its access key is the caller's (`vendor/server/src/request_authentication_sigv4a.rs`, `extract_akid` in `vendor/server/src/lib.rs`) | not reported yet |
| Core router | `MultiServiceRouter` runs an installed `RequestAuthorizer` after request authentication (`vendor/smithy-server-runtime/src/multi_service_router.rs`), which `iam-gate/` installs | not reported yet |
| ElastiCache | `DeleteReplicationGroup` is implemented | aws/aws-emulator#355 |
| STS | `AssumeRoleWithWebIdentity` verifies tokens against a service account key set registered for an EKS cluster's issuer | not reported yet |
| Elastic Load Balancing v2 | `DescribeTags`, `AddTags` and `RemoveTags` accept a listener's default rule, which `DescribeRules` reports | not reported yet |
| ElastiCache | `EMULATOR_ELASTICACHE_BACKEND=docker` serves a Valkey or Redis OSS group or cluster from a real server in a container (`src/state/redis_container.rs`) | not reported yet |
| Elastic Load Balancing v2 | A `forward` to an `instance` or `ip` target group passes the request on to the target over HTTP/1.1 (`src/service/target_proxy.rs`) instead of answering 502 | not reported yet |
| Elastic Load Balancing v2 | An `HTTPS` target group is reached over TLS without validating the target's certificate, and a forwarded request answers 502 when no inbound rule of the target's security groups admits the load balancer on the traffic port (`src/service/target_proxy.rs`, `traffic_admitted` in `src/state/health_check_firewall.rs`) | not reported yet |
| RDS | `EMULATOR_RDS_BACKEND=docker` serves a PostgreSQL DB instance with a real PostgreSQL server in a container (`src/state/data_plane/postgres_container.rs`) | not reported yet |
| Elastic Load Balancing v2 | `EMULATOR_ELB_LISTENER_SOCKETS=off` makes an Application Load Balancer publish an `elb.amazonaws.com` name instead of a socket's `host:port` | not reported yet |
| Elastic Load Balancing v2 | An unsigned form `POST /` on the emulator's endpoint (STS `AssumeRoleWithWebIdentity`) is not claimed as load balancer traffic, so it no longer answers `500 AmbiguousService` beside an ALB with a rule for `/` | not reported yet |
| STS | `GetCallerIdentity` reports EKS Pod Identity credentials as the associated role's session (`assumed-role/<role>/eks-...`) | not reported yet |
| EKS Auth | `AssumeRoleForPodIdentity` serves an EKS cluster of the emulator whose tokens a registered external Kubernetes cluster signs | not reported yet |
| EKS | `CreateNodegroup` launches `desiredSize` EC2 instances of the Amazon EKS optimized AL2023 AMI over the node group's subnets, tagged `eks:cluster-name` and `eks:nodegroup-name`, and `DeleteNodegroup` terminates them (`src/state/nodegroup_instances.rs`). No Auto Scaling group is created, and `UpdateNodegroupConfig` launches or terminates nothing | not reported yet |
| RDS, ElastiCache (and core listeners) | A container-backed endpoint closes a connection its VPC security groups do not admit (RDS `src/state/data_plane/postgres_container.rs`, ElastiCache `src/state/redis_container.rs`, EC2 `src/state/connection_firewall.rs`, `vendor/server/src/listeners.rs`) | not reported yet |
| Price List | EC2 instance products carry the `marketoption` attribute (`OnDemand`) | not reported yet |
| EKS | `EMULATOR_EKS_CONTROL_PLANE=external` leaves a cluster `CREATING` until an external provisioner registers its API server, and a cluster CA's signing key is kept on its record (`src/ops/create_cluster.rs`, `src/state/certificate_material.rs`) | not reported yet |
| EC2 | Each state an instance moves into is published on EventBridge as an `EC2 Instance State-change Notification`, including the states the clock moves it into | [aws/aws-emulator#549](https://github.com/aws/aws-emulator/issues/549) |

To find every patch: `git grep -n 'fork:' -- services vendor core`.

## Syncing with upstream

Merge `upstream/main` into `workspace`. `fork/` has no upstream counterpart, so only the patched
files can conflict. After the merge, run `render_router.py --check` and the workflow.
