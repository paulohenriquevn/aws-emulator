# Changelog

Changes this fork makes on top of aws/aws-emulator. Upstream keeps no changelog; its history is in git.

The format is based on [Keep a Changelog](https://keepachangelog.com/) and this project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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
