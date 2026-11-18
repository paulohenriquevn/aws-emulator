# Fork layer

This fork of [aws/aws-emulator](https://github.com/aws/aws-emulator) runs Terraform stacks built
on the community AWS modules (VPC, EKS, RDS, ElastiCache, CloudFront and more) without an AWS
account. Everything the fork adds lives in this directory, plus a few patches inside upstream crates, each marked with a `fork:` comment.

## What is here

| Path | What |
|---|---|
| `services.txt` | The 27 AWS service crates the router mounts. The upstream router builds every service and needs about 100 GB of build output; ours builds only these. |
| `render_router.py` | Generates `router/` from `services.txt`. `--check` fails when the generated files are stale. |
| `router/` | The slim router crate. Its binary is `aws-emulator-fork`. |
| `console/` | The read-only web console the binary serves at `/_console/`: a React app in `console/web` and the crate that embeds its build. |
| `acceptance/` | Eight Terraform fixtures (billing, compute, data, edge, network, operations, registry, security) and `run.sh`, which runs each one against a fresh emulator. |

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

## Known limits

- EKS Pod Identity is not served. Its token comes from the cluster's Kubernetes API server, which
  the emulator does not run, so EKS Auth is not mounted. IRSA (an IAM OIDC provider and
  `AssumeRoleWithWebIdentity`) works, and the `security` fixture covers it.
- EC2 publishes no `EC2 Instance State-change Notification` events. A rule on that pattern can be
  created and targeted, but only events a client sends with `PutEvents` reach it.
- Controllers that run inside the cluster (the AWS Load Balancer Controller, External Secrets,
  OpenCost) are not exercised here; the fixtures cover the AWS calls they make, not the
  controllers.

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

To find every patch: `git grep -n 'fork:' -- services vendor core`.

## Syncing with upstream

Merge `upstream/main` into `workspace`. `fork/` has no upstream counterpart, so only the patched
files can conflict. After the merge, run `render_router.py --check` and the workflow.
