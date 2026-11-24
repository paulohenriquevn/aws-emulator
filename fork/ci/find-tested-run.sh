#!/usr/bin/env bash
# Prints the id of the newest run of fork-acceptance.yml that passed every job on a commit
# with this git tree, or nothing. That run left an artifact named tested-tree-<tree>.
#
# A promotion (develop to main) and its version tag carry the tree that the pull request
# into develop already tested, so their runs reuse that run's binaries instead of spending
# another two hours compiling the same code.
#
# Only markers written by runs of this repository count: a pull request from a fork runs its
# own copy of the workflow and could upload a marker, with binaries, under any name.
#
# Needs GITHUB_REPOSITORY, GITHUB_REPOSITORY_ID and a gh authenticated with actions: read.
set -euo pipefail

tree=${1:?usage: find-tested-run.sh <git tree sha>}

gh api "repos/${GITHUB_REPOSITORY}/actions/artifacts?name=tested-tree-${tree}&per_page=100" \
  --jq "[.artifacts[]
         | select(.expired == false)
         | select(.workflow_run.head_repository_id == ${GITHUB_REPOSITORY_ID})]
        | sort_by(.created_at) | last | .workflow_run.id // empty"
