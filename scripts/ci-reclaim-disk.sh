#!/usr/bin/env bash
# Reclaim disk on CI runners before heavy cargo jobs.
#
# Written for the GitHub-hosted ubuntu-24.04 image, which ships ~25GB of
# preinstalled toolchains this repo never uses (dotnet, Android SDK, GHC,
# CodeQL). The workspace debug cache plus per-shard test-binary
# codegen exceeds the ~14GB that remains, which surfaces as `No space left on
# device` and linker Bus errors mid-shard.
#
# Every removal below is failure-tolerant because GitHub-hosted image contents
# can change independently of this script.
set -euo pipefail

timestamp() {
  date -u '+%Y-%m-%dT%H:%M:%SZ'
}

# Keep the image prune in the caller's foreground process: every caller may
# pull or start a container immediately after this script returns.
echo "runner disk reclaim started at $(timestamp)"
echo "before:"; df -h / | tail -1
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

echo "after:"; df -h / | tail -1
echo "runner disk reclaim finished at $(timestamp)"
