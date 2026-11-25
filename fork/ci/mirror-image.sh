#!/usr/bin/env bash
# Makes sure a registry this workflow can always pull from holds a copy of an image, and
# prints the copy's reference, pinned by the same digest.
#
# ECR Public answers anonymous pulls from the shared addresses of GitHub's runners with
# "Data limit exceeded". The source is therefore asked only when the copy does not hold the
# digest yet: once one run copied the image, no later run depends on ECR Public's quota.
#
# Usage: mirror-image.sh <source> <copy>
#   <source>  registry/repository:tag@sha256:<digest>
#   <copy>    the repository that keeps the copy, such as ghcr.io/<owner>/<name>
# Needs crane, logged in to the copy's registry. MIRROR_ATTEMPTS (6) and
# MIRROR_PAUSE_SECONDS (30, multiplied by the attempt) bound the retries of a refused copy.
set -euo pipefail

source_ref=${1:?usage: mirror-image.sh <source registry/repository:tag@sha256:digest> <copy repository>}
copy=${2:?usage: mirror-image.sh <source registry/repository:tag@sha256:digest> <copy repository>}
if [[ ! $source_ref =~ ^[^@]+:[^@:/]+@sha256:[0-9a-f]{64}$ ]]; then
  echo "mirror-image.sh: $source_ref is not registry/repository:tag@sha256:<digest>" >&2
  exit 2
fi
digest=${source_ref##*@}
tag=${source_ref%@*}
tag=${tag##*:}
attempts=${MIRROR_ATTEMPTS:-6}
pause=${MIRROR_PAUSE_SECONDS:-30}

if crane digest "$copy@$digest" >/dev/null 2>&1; then
  echo "$copy@$digest is already there" >&2
else
  attempt=1
  until crane copy "$source_ref" "$copy:$tag" >&2; do
    if [ "$attempt" -ge "$attempts" ]; then
      echo "::error::$source_ref could not be copied to $copy in $attempts attempts" >&2
      exit 1
    fi
    sleep $((attempt * pause))
    attempt=$((attempt + 1))
  done
  if ! crane digest "$copy@$digest" >/dev/null; then
    echo "::error::$copy does not hold $digest after the copy of $source_ref" >&2
    exit 1
  fi
fi
echo "$copy:$tag@$digest"
