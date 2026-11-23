# Changelog

Changes this fork makes on top of aws/aws-emulator. Upstream keeps no changelog; its history is in git.

The format is based on [Keep a Changelog](https://keepachangelog.com/) and this project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- The ECR registry endpoint accepts `docker push`: blob upload sessions (`POST`, chunked `PATCH`, `PUT` with digest check, monolithic `POST ?digest=`, status and cancel), and `PUT` of Docker schema 2 and OCI manifests and indexes, which checks that referenced blobs exist and honours tag immutability. A pushed image is listed by `ListImages` and `DescribeImages` and pulls back through the registry endpoint (#19)
- A push to a repository that does not exist creates it from the most specific repository creation template whose `appliedFor` includes `CREATE_ON_PUSH`, with the template's settings, as on ECR; without a matching template the push is refused with `NAME_UNKNOWN` (#19)
- `fork/eks-provisioner`: nodes pull images from the emulator's ECR with no `imagePullSecret`, as Amazon EKS nodes pull from ECR. Each node trusts the emulator's CA for the registry host (the host of `AWS_ENDPOINT_URL`) in containerd's `certs.d`, and its kubelet runs an image credential provider that gets a fresh ECR authorization token from the provisioner, which answers only a kubelet of the cluster. Nodes that joined earlier are configured on the next reconcile
- `EMULATOR_MAX_BODY_BYTES` raises the 16 MiB cap on request bodies, so a deployment that receives image layers, which `docker push` sends in one request, can accept them (#19)

## [0.3.1] - 2026-11-23

### Fixed
- An Application Load Balancer answers a request a browser sends from another site, and its CORS preflight, from its listener rules, as on AWS. The front door refused them with 403 `AccessDenied`, so a single-page app could not call an API behind a load balancer on another host (#17)

## [0.3.0] - 2026-11-23

### Added
- eks-provisioner: `--pod-identity-webhook-image` (or `POD_IDENTITY_WEBHOOK_IMAGE`) runs a copy of the pod identity webhook image, pinned by digest, on a host whose address ECR Public throttles (#15)
- `EMULATOR_EKS_CONTROL_PLANE=external` leaves an EKS cluster `CREATING` until a provisioner outside the emulator registers its API server with `PUT /_fork/eks/clusters/{name}/control-plane`, which turns it `ACTIVE` with that endpoint; `GET` on the same route answers the cluster's issuer, version and certificate authority, signing key included (#15)
- `fork/eks-provisioner` runs the control plane of each EKS cluster in kind, signed by the cluster's CA and issuing tokens as its OIDC issuer, joins each node group instance as a node at its private address, authenticates `aws eks get-token` tokens against the cluster's access entries, and deletes the kind cluster with the EKS cluster (#15)
- A version tag also publishes the provisioner binary, with its SHA-256, on the tag's GitHub release (#15)
- `fork/eks-provisioner` installs `amazon-eks-pod-identity-webhook` v0.6.19 on each cluster's control-plane node before the cluster turns `ACTIVE`, so a pod whose service account carries `eks.amazonaws.com/role-arn` gets IRSA as on Amazon EKS (#15)
- `fork/eks-provisioner` gives every container of a pod created outside `kube-system` the emulator's endpoint (`AWS_ENDPOINT_URL`, `AWS_ENDPOINT_URL_STS`) and CA (`AWS_CA_BUNDLE` and `SSL_CERT_FILE` with the public roots, `NODE_EXTRA_CA_CERTS`) through a mutating webhook it serves, and leaves alone a pod that sets `AWS_ENDPOINT_URL` (#15)
- `fork/eks-provisioner/e2e.sh` checks that a pod with an IRSA service account and no AWS variable of its own is the role for `aws sts get-caller-identity` (#15)

### Fixed
- eks-provisioner: a pod identity webhook that never becomes available is reported with what its pods wait for (scheduling, image pull, crash), and stopping on shutdown no longer logs `%!w(<nil>)` (#15)
- An EKS token is accepted for 15 minutes from its signing, as Amazon EKS and STS accept it: the emulator's STS and the provisioner refused it once its 60 second presign expiry passed, so a long `helm --wait` failed with `Unauthorized` (#15)
- A `kube-system` pod with an IRSA role, such as the AWS Load Balancer Controller, gets the emulator's endpoint and CA, instead of exchanging its token at the real STS (#15)
- `fork/eks-provisioner` tears down only the kind clusters it keeps a state directory for, and `e2e.sh` deletes only its own kind cluster and Docker network, so neither removes the clusters of another provisioner on the same host; the Docker network it creates for a VPC carries the VPC's CIDR in its name, so a second emulator's VPC with the same id gets a network of its own (#15)
- EC2 `DescribeTags` reports the tags `CreateTags` writes, each with its key, value, resource id and resource type, filtered by `key`, `value`, `resource-id`, `resource-type` and `tag:<key>` and paged by `MaxResults` (5 to 1000) and `NextToken`; it answered 501 for every request, so `aws_ec2_tag` could not apply (#16)

## [0.2.0] - 2026-11-20

### Added
- An Application Load Balancer passes a request its `forward` action sends to an `instance` or `ip` target on to that target over HTTP/1.1 and answers with the target's response, with 502 when the target cannot be reached and 504 when it does not answer in time; it answered 502 for every such request (#4)
- `EMULATOR_RDS_BACKEND=docker` serves an RDS for PostgreSQL DB instance with a real PostgreSQL server of its major version in a container, behind the instance's own endpoint, which keeps the instance status, `rds.force_ssl` and TLS with the emulator's certificate (#5)
- The `data` acceptance fixture logs in to its RDS instance with the managed secret over TLS and installs `pgcrypto` (#5)
- An `HTTPS` target group gets its requests over TLS, and the load balancer does not validate the target's certificate, as on AWS (#6)
- `EMULATOR_IAM_ENFORCE=on` judges every signed call to every service against the caller's identity-based policies at the action level, and `fork/kubernetes/run.sh` runs with it on, each role holding the policy its controller documents (#7)
- `EMULATOR_ELASTICACHE_BACKEND=docker` serves a Valkey or Redis OSS replication group (cluster mode disabled) or cache cluster with a real server of its engine and major version in a container, behind the endpoints the group publishes, with `AuthToken` as the server's password and TLS with the emulator's certificate under `TransitEncryptionEnabled` (#8)
- The `data` acceptance fixture writes a value to its Valkey group over TLS and reads it back (#8)
- `CreateNodegroup` launches the node group's `desiredSize` EC2 instances of the Amazon EKS optimized AMI, spread over its subnets and tagged `eks:cluster-name` and `eks:nodegroup-name`, and `DeleteNodegroup` terminates them (#9)
- `fork/kubernetes/run.sh` creates a managed node group and runs one kind worker per instance, with the instance's providerID, and checks the ALB targets all of them (#9)
- A container-backed RDS for PostgreSQL or ElastiCache endpoint closes a connection the DB instance's or the cache's VPC security groups do not admit before reading from it, when the source address is held by a network interface of the account; a source no network interface holds is let through (#11)
- `fork/kubernetes/run.sh` runs kind on a Docker network that is also the EKS cluster's VPC, gives the node container the node instance's private address, and sends a request through the ALB the AWS Load Balancer Controller creates and checks the echo pod answers it (#4)
- `fork/kubernetes/run.sh` checks that a pod reaches an RDS for PostgreSQL instance whose security group admits the nodes, and that an instance whose group admits nothing closes the pod's connection; each node sends its traffic to the emulator from its instance's address (#13)
- With `KEEP_CLUSTER=1`, `fork/kubernetes/run.sh` writes `environment.sh`, which a consumer sources to keep working on the emulator and the cluster (#13)
- A version tag publishes the emulator binary its workflow run tested, with its SHA-256, on the tag's GitHub release (#13)
- `fork/kubernetes/run.sh` translates a pod's TCP connection to the emulator to its node instance's address, so a database's security groups judge the node's groups (#13)
- `fork/kubernetes/run.sh` pulls the images its probes and the load balancer controller run once on the host, retrying when the registry refuses, and loads them into every node (#13)

### Fixed
- Removing a deleted DB instance's or cache's container no longer removes a newer container of the same name, such as the one for an instance or group created again with the same identifier; the emulator removes a container by its Docker id, and a container dropped while it starts is removed once `docker run` answers (#12)
- A request the ALB forwards reaches its target only when an inbound rule of the target's security groups admits the load balancer on the traffic port, and answers 502 otherwise, as health checks already did for the health-check port (#6)

## [0.1.0] - 2026-11-18

### Added
- `aws-emulator-fork`, a router that builds only the 22 service crates listed in `fork/services.txt` (#1)
- Seven Terraform acceptance fixtures and `fork/acceptance/run.sh` to run them (#1)
- SSM serves the EKS optimized AL2023 AMI parameters (`/aws/service/eks/optimized-ami/...`) for Kubernetes 1.31 to 1.37 (#1)
- S3 Control is part of the router, so the Terraform AWS provider 6.23+ can read and write S3 bucket tags (#1)
- ElastiCache implements `DeleteReplicationGroup`, so a Terraform stack holding a replication group can be destroyed (#1)
- A read-only web console at `/_console/` that lists the resources of 23 services, with a detail page per resource (#2)
- The router mounts CloudFormation, SQS, EventBridge, Shield and the Price List API, which Cluster API's IAM bootstrap, its instance-state events and the AWS Load Balancer Controller call, and which publishes the EC2 price list file OpenCost reads (#2)
- An `operations` acceptance fixture, and `run.sh` runs a fixture's `check.sh` after `apply` (#2)
- The `edge` fixture attaches a CloudFront response headers policy and checks an invalidation completes (#2)
- EC2 publishes an `EC2 Instance State-change Notification` on the default EventBridge bus for every state an instance moves into, without waiting for another EC2 call (#2)
- `PUT /_fork/eks/clusters/{name}/service-account-issuer-keys` registers a Kubernetes cluster's service account key set for an EKS cluster's OIDC issuer; STS `AssumeRoleWithWebIdentity` and EKS Auth `AssumeRoleForPodIdentity` then accept that cluster's tokens (#2)
- The router mounts EKS Auth (#2)
- `EMULATOR_ELB_LISTENER_SOCKETS=off` makes Application Load Balancers publish an `elb.amazonaws.com` DNS name, which a Kubernetes Ingress status accepts, instead of a socket's `host:port` (#2)
- With `RUST_LOG=info,access=debug` the binary logs each request it answers: operation, status, error code and duration (#2)
- `fork/kubernetes/run.sh` runs IRSA, EKS Pod Identity, the AWS Load Balancer Controller, External Secrets and OpenCost in a kind cluster against the emulator (#2)

### Changed
- The fork's layer lives in `fork/`, its router crate is `aws-emulator-fork-router`, its binary is `aws-emulator-fork`, and `run.sh` reads `ACCEPTANCE_WORK` and `ACCEPTANCE_PORT`; the names no longer carry the downstream project's name (#2)

### Fixed
- An unsigned query-protocol call to the emulator's endpoint, such as STS `AssumeRoleWithWebIdentity`, no longer answers `500 AmbiguousService` while one Application Load Balancer has a rule for `/` (#2)
- Elastic Load Balancing v2 `DescribeTags`, `AddTags` and `RemoveTags` accept a listener's default rule instead of answering `RuleNotFound`, so the AWS Load Balancer Controller can reconcile an Ingress (#2)
- STS `GetCallerIdentity` reports credentials from EKS Auth `AssumeRoleForPodIdentity` as the associated role's session, not as the emulator's own user (#2)
- The Price List API's EC2 instance products carry `marketoption` `OnDemand`, without which OpenCost prices no node from the price list (#2)
- EC2 accepts `CreateTags` and `DeleteTags` on network ACLs, the default one included (#1)
- EC2 revokes an all-protocols security group rule whatever port range the request names (#1)
- ACM reports the same DNS validation record for a wildcard name as for its apex (#1)
- Elastic Load Balancing v2 calls reach the ELBv2 model; the router lacked the Classic front door that serves the shared `elasticloadbalancing` signing name (#1)
- An RDS instance or cluster created with a `Port` listens on it, on another loopback address when 127.0.0.1 already serves that port, so `Endpoint.Port` matches the configuration and Terraform plans no change (#1)
- The emulator's TLS certificate covers `000000000000.localhost`, so clients that prefix the host with the account id still verify it (#1)

[Unreleased]: https://github.com/paulohenriquevn/aws-emulator/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/paulohenriquevn/aws-emulator/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/paulohenriquevn/aws-emulator/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/paulohenriquevn/aws-emulator/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/paulohenriquevn/aws-emulator/releases/tag/v0.1.0
