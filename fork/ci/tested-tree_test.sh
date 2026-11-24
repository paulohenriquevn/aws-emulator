#!/usr/bin/env bash
# Tests for tested-tree.sh: a commit that changes only fork/CHANGELOG.md keeps the tree the
# acceptance run vouches for, and any other change gives a new one.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"
git init -q .
git config user.email ci@example.invalid
git config user.name ci
mkdir -p fork/ci src
echo one >src/lib.rs
echo "# Changelog" >fork/CHANGELOG.md
git add . && git commit -qm base
base=$("$here/tested-tree.sh")

failures=0
check() { # name expected got
  if [ "$3" = "$2" ]; then echo "ok   $1"; else echo "FAIL $1: want '$2', got '$3'"; failures=$((failures + 1)); fi
}
differs() { # name got
  if [ "$2" != "$base" ]; then echo "ok   $1"; else echo "FAIL $1: the tree did not change"; failures=$((failures + 1)); fi
}

check "the tree leaves the index of the checkout alone" "" "$(git status --porcelain)"

echo "## [0.0.1]" >>fork/CHANGELOG.md
git commit -qam release
check "a commit that changes only the changelog keeps the tree" "$base" "$("$here/tested-tree.sh")"

echo two >src/lib.rs
git commit -qam code
differs "a code change gives a new tree" "$("$here/tested-tree.sh")"

exit "$failures"
