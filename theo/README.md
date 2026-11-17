# theo layer

This fork of [aws/aws-emulator](https://github.com/aws/aws-emulator) runs theo's Terraform modules
(`theo-infra-modules`, `theo-infra-live`) without an AWS account. Everything theo adds lives in
this directory, plus a few patches inside upstream crates, each marked with a `theo:` comment.

## What is here

| Path | What |
|---|---|
| `services.txt` | The 22 AWS service crates theo uses. The upstream router builds every service and needs about 100 GB of build output; ours builds only these. |
| `render_router.py` | Generates `router/` from `services.txt`. `--check` fails when the generated files are stale. |
| `router/` | The slim router crate. Its binary is `theo-aws-emulator`. |
| `acceptance/` | Seven Terraform fixtures (billing, compute, data, edge, network, registry, security) and `run.sh`, which runs each one against a fresh emulator. |

## Usage

```bash
python3 theo/render_router.py            # after editing services.txt
cargo build -p theo-aws-emulator-router
theo/acceptance/run.sh target/debug/theo-aws-emulator           # every fixture
theo/acceptance/run.sh target/debug/theo-aws-emulator network   # one fixture
```

A fixture passes when `apply` succeeds, a second `plan` shows no changes, and `destroy` leaves
an empty state. `run.sh` needs Terraform 1.10 or later and network access to download the AWS
provider.

The EC2 crate needs several GB of memory to compile. The workflow
`.github/workflows/theo-acceptance.yml` runs the regression tests, the fixtures and the full suites
of the patched services on every pull request.

## Patches to upstream crates

Each patch starts from a failing regression test, is reported upstream, and is dropped once
upstream ships the fix.

| Service | Gap | Upstream |
|---|---|---|
| EC2 | Tags on network ACLs | aws/aws-emulator#76 |
| EC2 | Revoking an all-protocols rule that names a port range | aws/aws-emulator#46 |
| ACM | One DNS validation CNAME for a wildcard and its apex | not filed yet |
| SSM | EKS optimized AL2023 AMI parameters | not filed yet |
| TLS | The certificate covers `000000000000.localhost`, the host S3 Control dials | aws/aws-emulator#56 |
| RDS (and core listeners) | An instance listens on its configured `Port` when free, so `Endpoint.Port` matches it | not filed yet |
| ElastiCache | `DeleteReplicationGroup` is implemented | not filed yet |

To find every patch: `git grep -n 'theo:' -- services vendor core`.

## Syncing with upstream

Merge `upstream/main` into `workspace`. `theo/` has no upstream counterpart, so only the patched
files can conflict. After the merge, run `render_router.py --check` and the workflow.
