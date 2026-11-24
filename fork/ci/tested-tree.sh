#!/usr/bin/env bash
# Prints the git tree that an acceptance run vouches for: HEAD's tree without
# fork/CHANGELOG.md. The release commit of a version only moves the changelog's Unreleased
# section under the version, so it keeps the tree the pull request already tested and its
# promotion and tag reuse that run (find-tested-run.sh) instead of testing the same code again.
set -euo pipefail

index=$(mktemp)
trap 'rm -f "$index"' EXIT
export GIT_INDEX_FILE=$index
git read-tree HEAD
git rm -q --cached --ignore-unmatch fork/CHANGELOG.md
git write-tree
