# Fork layer

This fork of [aws/aws-emulator](https://github.com/aws/aws-emulator) runs Terraform stacks built
on the community AWS modules (VPC, EKS, RDS, ElastiCache, CloudFront and more) without an AWS
account. Everything the fork adds lives in this directory, plus a few patches inside upstream crates, each marked with a `fork:` comment.

## What is here

| Path | What |
|---|---|
| `services.txt` | The 28 AWS service crates the router mounts. The upstream router builds every service and needs about 100 GB of build output; ours builds only these. |
| `render_router.py` | Generates `router/` from `services.txt`. `--check` fails when the generated files are stale. |
| `router/` | The slim router crate. Its binary is `aws-emulator-fork`. |
| `console/` | The read-only web console the binary serves at `/_console/`: a React app in `console/web` and the crate that embeds its build. |
| `acceptance/` | Eight Terraform fixtures (billing, compute, data, edge, network, operations, registry, security) and `run.sh`, which runs each one against a fresh emulator. |
| `service-account-issuers/` | The route that registers a Kubernetes cluster's service account key set for an EKS cluster of the emulator. |
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
| `pod-identity` | A pod whose service account has a pod identity association gets the role's credentials from the EKS Pod Identity Agent. |
| `load-balancer` | The AWS Load Balancer Controller, on IRSA, turns an Ingress into an ALB in the emulator with the kind node's instance registered as a target (instance targets: a kind pod has no VPC network interface for an IP target), the target healthy, and a request to the ALB's name answered by the echo pod's nginx. |
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

It needs docker, kind, kubectl, helm, the AWS CLI v2, curl, jq, openssl and git, and network
access for images and charts. The emulator listens on the gateway of kind's Docker network
(`-b`), and every pod that calls it mounts its CA. The `kubernetes` job of the workflow runs it.

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
| ACM | One DNS validation CNAME for a wildcard and its apex | aws/aws-emulator#536 |
| SSM | EKS optimized AL2023 AMI parameters | aws/aws-emulator#535 |
| TLS | The certificate covers `000000000000.localhost`, the host S3 Control dials | aws/aws-emulator#56 |
| RDS (and core listeners) | An instance created with a `Port` listens on it, on another loopback address when 127.0.0.1 already serves it, so `Endpoint.Port` matches it | aws/aws-emulator#537 |
| ElastiCache | `DeleteReplicationGroup` is implemented | aws/aws-emulator#355 |
| STS | `AssumeRoleWithWebIdentity` verifies tokens against a service account key set registered for an EKS cluster's issuer | not reported yet |
| Elastic Load Balancing v2 | `DescribeTags`, `AddTags` and `RemoveTags` accept a listener's default rule, which `DescribeRules` reports | not reported yet |
| Elastic Load Balancing v2 | A `forward` to an `instance` or `ip` target group passes the request on to the target over HTTP/1.1 (`src/service/target_proxy.rs`) instead of answering 502 | not reported yet |
| RDS | `EMULATOR_RDS_BACKEND=docker` serves a PostgreSQL DB instance with a real PostgreSQL server in a container (`src/state/data_plane/postgres_container.rs`) | not reported yet |
| Elastic Load Balancing v2 | `EMULATOR_ELB_LISTENER_SOCKETS=off` makes an Application Load Balancer publish an `elb.amazonaws.com` name instead of a socket's `host:port` | not reported yet |
| Elastic Load Balancing v2 | An unsigned form `POST /` on the emulator's endpoint (STS `AssumeRoleWithWebIdentity`) is not claimed as load balancer traffic, so it no longer answers `500 AmbiguousService` beside an ALB with a rule for `/` | not reported yet |
| STS | `GetCallerIdentity` reports EKS Pod Identity credentials as the associated role's session (`assumed-role/<role>/eks-...`) | not reported yet |
| EKS Auth | `AssumeRoleForPodIdentity` serves an EKS cluster of the emulator whose tokens a registered external Kubernetes cluster signs | not reported yet |
| Price List | EC2 instance products carry the `marketoption` attribute (`OnDemand`) | not reported yet |
| EC2 | Each state an instance moves into is published on EventBridge as an `EC2 Instance State-change Notification`, including the states the clock moves it into | [aws/aws-emulator#549](https://github.com/aws/aws-emulator/issues/549) |

To find every patch: `git grep -n 'fork:' -- services vendor core`.

## Syncing with upstream

Merge `upstream/main` into `workspace`. `fork/` has no upstream counterpart, so only the patched
files can conflict. After the merge, run `render_router.py --check` and the workflow.
