#!/usr/bin/env bash
# Reclaim disk on CI runners before heavy cargo jobs.
#
# GitHub-hosted image capacity changes independently of this repository. A
# 2026-09 image supplied 86 GiB free before setup while the workspace test
# build consumed about 21 GiB. Reclaim only when less than 60 GiB is available:
# that keeps roughly twice the measured build growth as headroom without
# spending minutes deleting preinstalled toolchains from already-roomy images.
#
# Every removal below is failure-tolerant because GitHub-hosted image contents
# can change independently of this script.
set -euo pipefail

min_free_kib="${CI_RECLAIM_MIN_FREE_KIB:-62914560}"
if [[ ! "$min_free_kib" =~ ^[0-9]+$ ]]; then
  echo "CI_RECLAIM_MIN_FREE_KIB must be an unsigned integer, got '${min_free_kib}'" >&2
  exit 2
fi

timestamp() {
  date -u '+%Y-%m-%dT%H:%M:%SZ'
}

# Keep the image prune in the caller's foreground process: every caller may
# pull or start a container immediately after this script returns.
echo "runner disk reclaim started at $(timestamp)"
echo "before:"
df -h / | tail -1 || true

available_kib=""
if measured_kib="$(df -Pk / 2>/dev/null | awk 'NR == 2 { print $4 }')" \
  && [[ "$measured_kib" =~ ^[0-9]+$ ]]; then
  available_kib="$measured_kib"
  echo "runner disk available KiB ${available_kib}; reclaim floor KiB ${min_free_kib}"
fi

if [[ -n "$available_kib" ]] && ((available_kib >= min_free_kib)); then
  echo "runner disk reclaim skipped: available capacity meets the floor"
  echo "runner disk reclaim finished at $(timestamp)"
  exit 0
fi

if [[ -z "$available_kib" ]]; then
  echo "runner disk capacity probe failed; reclaiming conservatively" >&2
else
  echo "runner disk capacity is below the floor; reclaiming synchronously"
fi

sudo rm -rf /usr/share/dotnet /usr/local/lib/android /opt/ghc \
  /opt/hostedtoolcache/CodeQL /usr/local/share/boost 2>/dev/null || true

echo "docker image prune started at $(timestamp)"
prune_status=0
if sudo docker image prune --all --force; then
  prune_status=0
else
  prune_status=$?
fi
echo "docker image prune finished at $(timestamp)"
echo "docker image prune exit status ${prune_status}"
if ((prune_status != 0)); then
  echo "docker image prune failed; continuing with best-effort runner cleanup" >&2
fi

echo "after:"; df -h / | tail -1 || true
echo "runner disk reclaim finished at $(timestamp)"
