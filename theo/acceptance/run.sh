#!/usr/bin/env bash
# Run every Terraform fixture under theo/acceptance against a fresh emulator.
#
# A fixture passes only when all four steps succeed:
#   1. apply      the resources are created
#   2. plan       a second plan shows NO changes (-detailed-exitcode 0); a diff means
#                 the emulator read back something different from what it accepted
#   3. destroy    every resource is deleted
#   4. empty      nothing the fixture created is left behind (checked by the destroy
#                 plan's own state, which must be empty)
#
# Usage: theo/acceptance/run.sh <emulator-binary> [fixture ...]
# Exit status: 0 when every selected fixture passes, 1 otherwise.
set -euo pipefail

if [[ $# -lt 1 || ! -x $1 ]]; then
  echo "usage: $0 <emulator-binary> [fixture ...]  (binary not found or not executable: ${1:-<none>})" >&2
  exit 2
fi
BIN=$(realpath "$1"); shift
HERE=$(cd "$(dirname "$0")" && pwd)
mapfile -t FIXTURES < <(if [[ $# -gt 0 ]]; then printf '%s\n' "$@"; else
  find "$HERE" -mindepth 1 -maxdepth 1 -type d ! -name '_*' -printf '%f\n' | sort; fi)

WORK=${THEO_ACCEPTANCE_WORK:-$(mktemp -d)}
export TF_PLUGIN_CACHE_DIR=${TF_PLUGIN_CACHE_DIR:-$WORK/plugin-cache}
mkdir -p "$TF_PLUGIN_CACHE_DIR"
PORT=${THEO_ACCEPTANCE_PORT:-9397}

# Never let a fixture find real credentials: everything AWS comes from the emulator.
unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN AWS_PROFILE AWS_DEFAULT_PROFILE

start_emulator() {
  local home=$1
  mkdir -p "$home"
  HOME=$home "$BIN" -p "$PORT" --telemetry off >"$home/emulator.log" 2>&1 &
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
}
trap stop_emulator EXIT

failed=0
printf '%-10s %-8s %-8s %-8s %s\n' FIXTURE APPLY REPLAN DESTROY LOG
for fx in "${FIXTURES[@]}"; do
  dir=$WORK/$fx
  rm -rf "$dir"; mkdir -p "$dir"
  cp -rL "$HERE/$fx/." "$dir/src"
  log=$dir/terraform.log
  apply=- replan=- destroy=-
  start_emulator "$dir/home"
  (
    cd "$dir/src"
    terraform init -input=false -no-color >>"$log" 2>&1
  ) || { apply=INIT-FAIL; }
  if [[ $apply == - ]]; then
    if (cd "$dir/src" && terraform apply -auto-approve -input=false -no-color >>"$log" 2>&1); then
      apply=ok
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
  [[ $apply == ok && $replan == ok && $destroy == ok ]] || failed=1
  printf '%-10s %-8s %-8s %-8s %s\n' "$fx" "$apply" "$replan" "$destroy" "$log"
done
exit $failed
