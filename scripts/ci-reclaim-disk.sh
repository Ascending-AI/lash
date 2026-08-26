#!/usr/bin/env bash
# Reclaim disk on CI runners before heavy cargo jobs.
#
# Written for the GitHub-hosted ubuntu-24.04 image, which ships ~25GB of
# preinstalled toolchains this repo never uses (dotnet, Android SDK, GHC,
# CodeQL). The workspace debug cache plus sccache plus per-shard test-binary
# codegen exceeds the ~14GB that remains, which surfaces as `No space left on
# device` and linker Bus errors mid-shard.
#
# Every removal below is failure-tolerant because GitHub-hosted image contents
# can change independently of this script.
set -euo pipefail

# The removals run detached: they free ~25GB well before the compile needs
# it, and GitHub-hosted runners keep background processes alive across the
# job's remaining steps, so blocking the job for 60-90s of rm/prune bought
# nothing. Nothing later in any job reads the reclaimed paths.
echo "before:"; df -h / | tail -1
sudo bash -c 'nohup sh -c "
  rm -rf /usr/share/dotnet /usr/local/lib/android /opt/ghc \
    /opt/hostedtoolcache/CodeQL /usr/local/share/boost 2>/dev/null
  docker image prune --all --force >/dev/null 2>&1
" >/dev/null 2>&1 &'
echo "reclaim running in background"
