#!/usr/bin/env bash
# Run every Terraform fixture under fork/acceptance against a fresh emulator.
#
# A fixture passes only when every step succeeds:
#   1. apply      the resources are created
#   2. check      the fixture's own check.sh, when it has one, exercises what Terraform
#                 cannot (an invalidation, an event delivered to a queue). It runs in
#                 the fixture directory with the emulator's AWS_* environment, after
#                 apply, and reads the outputs with `terraform output`
#   3. plan       a second plan shows NO changes (-detailed-exitcode 0); a diff means
#                 the emulator read back something different from what it accepted
#   4. destroy    every resource is deleted
#   5. empty      nothing the fixture created is left behind (checked by the destroy
#                 plan's own state, which must be empty)
#
# Usage: fork/acceptance/run.sh <emulator-binary | docker:IMAGE> [fixture ...]
#   <emulator-binary>  this fork's aws-emulator-fork, over HTTPS with its own CA
#   docker:IMAGE       an emulator image that serves plain HTTP on container port 4566
#                      (Floci, LocalStack), started fresh for every fixture
# Exit status: 0 when every selected fixture passes, 1 otherwise.
set -euo pipefail

TARGET=${1:-}
if [[ $TARGET == docker:?* ]]; then
  IMAGE=${TARGET#docker:}
  BIN=
elif [[ -n $TARGET && -x $TARGET ]]; then
  IMAGE=
  BIN=$(realpath "$TARGET")
else
  echo "usage: $0 <emulator-binary | docker:IMAGE> [fixture ...]  (not an executable or docker:IMAGE: ${TARGET:-<none>})" >&2
  exit 2
fi
shift
HERE=$(cd "$(dirname "$0")" && pwd)
mapfile -t FIXTURES < <(if [[ $# -gt 0 ]]; then printf '%s\n' "$@"; else
  find "$HERE" -mindepth 1 -maxdepth 1 -type d ! -name '_*' -printf '%f\n' | sort; fi)

WORK=${ACCEPTANCE_WORK:-$(mktemp -d)}
export TF_PLUGIN_CACHE_DIR=${TF_PLUGIN_CACHE_DIR:-$WORK/plugin-cache}
mkdir -p "$TF_PLUGIN_CACHE_DIR"
PORT=${ACCEPTANCE_PORT:-9397}

# Never let a fixture find real credentials: everything AWS comes from the emulator.
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN AWS_PROFILE AWS_DEFAULT_PROFILE

start_container() {
  local home=$1
  mkdir -p "$home"
  EMU_CONTAINER=acceptance-$(basename "$(dirname "$home")")
  docker rm -f "$EMU_CONTAINER" >/dev/null 2>&1 || true
  # The Docker socket lets the image start the real engines it backs RDS, ElastiCache
  # and EKS with.
  docker run -d --name "$EMU_CONTAINER" -p "$PORT:4566" \
    -v /var/run/docker.sock:/var/run/docker.sock "$IMAGE" >"$home/container.id"
  for _ in $(seq 120); do
    if [[ $(curl -s -o /dev/null -w '%{http_code}' "http://localhost:$PORT/") != 000 ]]; then
      export AWS_ENDPOINT_URL=http://localhost:$PORT
      export AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test
      export AWS_EC2_METADATA_DISABLED=true AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1
      unset AWS_CA_BUNDLE AWS_SHARED_CREDENTIALS_FILE AWS_CONFIG_FILE
      return 0
    fi
    sleep 1
  done
  docker logs "$EMU_CONTAINER" >"$home/emulator.log" 2>&1 || true
  echo "container $EMU_CONTAINER did not answer on port $PORT; log: $home/emulator.log" >&2
  return 1
}

start_emulator() {
  if [[ -n $IMAGE ]]; then start_container "$@"; return; fi
  local home=$1
  mkdir -p "$home"
  # RDS for PostgreSQL runs real PostgreSQL servers in containers (fork/README.md).
  HOME=$home EMULATOR_RDS_BACKEND=${EMULATOR_RDS_BACKEND:-docker} "$BIN" -p "$PORT" --telemetry off >"$home/emulator.log" 2>&1 &
  EMU_PID=$!
  local ca=$home/.aws/emulator/$PORT/ca.pem
  for _ in $(seq 60); do
    if [[ -f $ca ]] && curl -sf --cacert "$ca" "https://127.0.0.1:$PORT/__admin/health" >/dev/null; then
      # localhost, not 127.0.0.1: S3 Control prefixes the host with the account id, and
      # only a name under localhost (the certificate names 000000000000.localhost) resolves.
      export AWS_ENDPOINT_URL=https://localhost:$PORT
      export AWS_CA_BUNDLE=$ca
      export AWS_SHARED_CREDENTIALS_FILE=$home/.aws/emulator/$PORT/emulator_credentials
      export AWS_CONFIG_FILE=$home/.aws/emulator/$PORT/config
      export AWS_EC2_METADATA_DISABLED=true AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1
      return 0
    fi
    kill -0 "$EMU_PID" 2>/dev/null || break
    sleep 1
  done
  echo "emulator did not become healthy on port $PORT; log: $home/emulator.log" >&2
  return 1
}

stop_emulator() {
  if [[ -n ${EMU_PID:-} ]]; then kill "$EMU_PID" 2>/dev/null || true; wait "$EMU_PID" 2>/dev/null || true; fi
  EMU_PID=
  if [[ -n ${EMU_CONTAINER:-} ]]; then
    docker logs "$EMU_CONTAINER" >"$WORK/${EMU_CONTAINER#acceptance-}/home/emulator.log" 2>&1 || true
    docker rm -f "$EMU_CONTAINER" >/dev/null 2>&1 || true
  fi
  EMU_CONTAINER=
}
trap stop_emulator EXIT

failed=0
printf '%-10s %-8s %-8s %-8s %-8s %s\n' FIXTURE APPLY CHECK REPLAN DESTROY LOG
for fx in "${FIXTURES[@]}"; do
  dir=$WORK/$fx
  rm -rf "$dir"; mkdir -p "$dir"
  cp -rL "$HERE/$fx/." "$dir/src"
  log=$dir/terraform.log
  apply=- check=- replan=- destroy=-
  if ! start_emulator "$dir/home"; then
    failed=1
    printf '%-10s %-8s %-8s %-8s %-8s %s\n' "$fx" START-FAIL - - - "$dir/home/emulator.log"
    continue
  fi
  (
    cd "$dir/src"
    terraform init -input=false -no-color >>"$log" 2>&1
  ) || { apply=INIT-FAIL; }
  if [[ $apply == - ]]; then
    if (cd "$dir/src" && terraform apply -auto-approve -input=false -no-color >>"$log" 2>&1); then
      apply=ok
      if [[ -f $dir/src/check.sh ]]; then
        if (cd "$dir/src" && bash ./check.sh >>"$log" 2>&1); then check=ok; else check=FAIL; fi
      fi
      set +e
      (cd "$dir/src" && terraform plan -detailed-exitcode -input=false -no-color >>"$log" 2>&1)
      rc=$?
      set -e
      case $rc in 0) replan=ok ;; 2) replan=DRIFT ;; *) replan=FAIL ;; esac
    else
      apply=FAIL
    fi
    if (cd "$dir/src" && terraform destroy -auto-approve -input=false -no-color >>"$log" 2>&1) \
       && [[ -z $(cd "$dir/src" && terraform state list 2>/dev/null) ]]; then
      destroy=ok
    else
      destroy=FAIL
    fi
  fi
  stop_emulator
  [[ $apply == ok && $check != FAIL && $replan == ok && $destroy == ok ]] || failed=1
  printf '%-10s %-8s %-8s %-8s %-8s %s\n' "$fx" "$apply" "$check" "$replan" "$destroy" "$log"
done
exit $failed
