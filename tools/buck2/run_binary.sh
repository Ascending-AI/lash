#!/usr/bin/env bash
set -euo pipefail

while [[ "$1" != -- ]]; do
  name="$1"
  if [[ ! -v "$name" ]]; then
    export "$name=$2"
  fi
  shift 2
done
shift
exec "$@"
