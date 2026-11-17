# Changelog

Changes theo makes on top of aws/aws-emulator. Upstream keeps no changelog; its history is in git.

The format is based on [Keep a Changelog](https://keepachangelog.com/) and this project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- `theo-aws-emulator`, a router that builds only the 21 services theo's Terraform modules use (#1)
- Seven Terraform acceptance fixtures and `theo/acceptance/run.sh` to run them (#1)
- SSM serves the EKS optimized AL2023 AMI parameters (`/aws/service/eks/optimized-ami/...`) for Kubernetes 1.31 to 1.37 (#1)
- S3 Control is part of the router, so the Terraform AWS provider 6.23+ can read and write S3 bucket tags (#1)

### Fixed
- EC2 accepts `CreateTags` and `DeleteTags` on network ACLs, the default one included (#1)
- EC2 revokes an all-protocols security group rule whatever port range the request names (#1)
- ACM reports the same DNS validation record for a wildcard name as for its apex (#1)
- The emulator's TLS certificate covers `000000000000.localhost`, so clients that prefix the host with the account id still verify it (#1)
