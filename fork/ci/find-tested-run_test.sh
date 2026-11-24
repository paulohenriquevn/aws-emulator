#!/usr/bin/env bash
# Tests for find-tested-run.sh, with a fake gh that serves canned artifact listings.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/bin"
cat >"$work/bin/gh" <<'FAKE'
#!/usr/bin/env bash
# Prints the listing the test put in FAKE_ARTIFACTS, filtered with the --jq the script passed.
jq_filter=""
while [ $# -gt 0 ]; do
  case "$1" in --jq) jq_filter=$2; shift 2 ;; *) shift ;; esac
done
jq -r "$jq_filter" <<<"$FAKE_ARTIFACTS"
FAKE
chmod +x "$work/bin/gh"
export PATH="$work/bin:$PATH" GITHUB_REPOSITORY=owner/repo GITHUB_REPOSITORY_ID=42

failures=0
check() { # name expected listing
  local got
  got=$(FAKE_ARTIFACTS=$3 "$here/find-tested-run.sh" 0123abcd)
  if [ "$got" = "$2" ]; then echo "ok   $1"; else echo "FAIL $1: want '$2', got '$got'"; failures=$((failures + 1)); fi
}
artifact() { # run_id repository_id expired created_at
  printf '{"expired":%s,"created_at":"%s","workflow_run":{"id":%s,"head_repository_id":%s}}' "$3" "$4" "$1" "$2"
}

check "no marker means no run to reuse" "" '{"artifacts":[]}'
check "a marker from this repository names its run" "111" \
  "{\"artifacts\":[$(artifact 111 42 false 2026-11-23T10:00:00Z)]}"
check "a marker from a fork is ignored" "" \
  "{\"artifacts\":[$(artifact 222 99 false 2026-11-23T10:00:00Z)]}"
check "an expired marker is ignored" "" \
  "{\"artifacts\":[$(artifact 333 42 true 2026-11-23T10:00:00Z)]}"
check "the newest marker wins" "555" \
  "{\"artifacts\":[$(artifact 444 42 false 2026-11-22T10:00:00Z),$(artifact 555 42 false 2026-11-23T10:00:00Z)]}"

[ "$failures" -eq 0 ]
