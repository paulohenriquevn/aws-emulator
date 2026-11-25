#!/usr/bin/env bash
# Tests for mirror-image.sh, with a fake crane whose registry is a file of digests.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/bin"
cat >"$work/bin/crane" <<'FAKE'
#!/usr/bin/env bash
# digest <ref>: answers when FAKE_STATE/held lists the ref's digest.
# copy <source> <target>: refused with ECR Public's answer while FAKE_STATE/refuse exists;
# otherwise the copy holds the source's digest from then on. Every copy is logged.
case "$1" in
  digest)
    grep -qxF "${2##*@}" "$FAKE_STATE/held" 2>/dev/null || exit 1
    echo "${2##*@}"
    ;;
  copy)
    echo "$2 $3" >>"$FAKE_STATE/copies"
    if [ -e "$FAKE_STATE/refuse" ]; then
      echo "Error response from daemon: error from registry: Data limit exceeded" >&2
      exit 1
    fi
    echo "${2##*@}" >>"$FAKE_STATE/held"
    ;;
  *) echo "the fake crane does not know $1" >&2; exit 64 ;;
esac
FAKE
chmod +x "$work/bin/crane"
export PATH="$work/bin:$PATH" MIRROR_ATTEMPTS=3 MIRROR_PAUSE_SECONDS=0

digest=sha256:$(printf 'a%.0s' $(seq 64))
source_ref=public.ecr.aws/aws-cli/aws-cli:2.37.8@$digest
copy=ghcr.io/owner/aws-cli

failures=0
check() { # name expected got
  if [ "$3" = "$2" ]; then echo "ok   $1"; else echo "FAIL $1: want '$2', got '$3'"; failures=$((failures + 1)); fi
}
fresh() { # a registry state: [held] [refuse]
  FAKE_STATE=$(mktemp -d -p "$work")
  export FAKE_STATE
  : >"$FAKE_STATE/copies"
  for flag in "$@"; do
    case $flag in held) echo "$digest" >"$FAKE_STATE/held" ;; refuse) : >"$FAKE_STATE/refuse" ;; esac
  done
}
run() { # prints "<exit status> <stdout>"
  local out status=0
  out=$("$here/mirror-image.sh" "$@" 2>/dev/null) || status=$?
  echo "$status $out"
}

fresh
check "an image the copy lacks is copied, and the copy's pinned reference printed" \
  "0 $copy:2.37.8@$digest" "$(run "$source_ref" "$copy")"
check "the copy was made once, under the source's tag" "$source_ref $copy:2.37.8" "$(cat "$FAKE_STATE/copies")"

fresh held refuse
check "a copy that holds the digest is used while the source refuses pulls" \
  "0 $copy:2.37.8@$digest" "$(run "$source_ref" "$copy")"
check "the source is not asked when the copy holds the digest" "" "$(cat "$FAKE_STATE/copies")"

fresh refuse
check "a refused copy of an image the copy lacks fails" "1 " "$(run "$source_ref" "$copy")"
check "a refused copy is tried MIRROR_ATTEMPTS times" "3" "$(wc -l <"$FAKE_STATE/copies" | tr -d ' ')"

fresh
check "a source without a digest is refused before any copy" "2 " "$(run public.ecr.aws/aws-cli/aws-cli:2.37.8 "$copy")"
check "a source without a tag is refused before any copy" "2 " "$(run "public.ecr.aws/aws-cli/aws-cli@$digest" "$copy")"
check "nothing was copied for a malformed source" "" "$(cat "$FAKE_STATE/copies")"

[ "$failures" -eq 0 ]
