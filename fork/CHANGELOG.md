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
- The router mounts CloudFormation, SQS, EventBridge, Shield and the Price List API, which Cluster API's IAM bootstrap, its instance-state events, the AWS Load Balancer Controller and OpenCost call (#2)
- An `operations` acceptance fixture, and `run.sh` runs a fixture's `check.sh` after `apply` (#2)
- The `edge` fixture attaches a CloudFront response headers policy and checks an invalidation completes (#2)

### Changed
- The fork's layer lives in `fork/`, its router crate is `aws-emulator-fork-router`, its binary is `aws-emulator-fork`, and `run.sh` reads `ACCEPTANCE_WORK` and `ACCEPTANCE_PORT`; the names no longer carry the downstream project's name (#2)

### Fixed
- EC2 accepts `CreateTags` and `DeleteTags` on network ACLs, the default one included (#1)
- EC2 revokes an all-protocols security group rule whatever port range the request names (#1)
- ACM reports the same DNS validation record for a wildcard name as for its apex (#1)
- Elastic Load Balancing v2 calls reach the ELBv2 model; the router lacked the Classic front door that serves the shared `elasticloadbalancing` signing name (#1)
- An RDS instance or cluster created with a `Port` listens on it, on another loopback address when 127.0.0.1 already serves that port, so `Endpoint.Port` matches the configuration and Terraform plans no change (#1)
- The emulator's TLS certificate covers `000000000000.localhost`, so clients that prefix the host with the account id still verify it (#1)
