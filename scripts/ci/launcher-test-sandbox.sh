#!/usr/bin/env bash

# Sourced before a launcher test creates fixtures. The launcher keeps its
# real per-user ownership namespace; only the test's mount/PID/network view
# changes. A child launcher cannot reach host services or host processes.
launcher_test_sandbox() {
  local namespace
  namespace="$(readlink /proc/self/ns/mnt)"
  if [[ "${LASH_LAUNCHER_TEST_NAMESPACE:-}" == "$namespace" ]]; then
    return
  fi
  if ! command -v bwrap >/dev/null 2>&1; then
    printf 'launcher tests require bubblewrap (install the bubblewrap package)\n' >&2
    exit 2
  fi
  # shellcheck disable=SC2016 # Expansion belongs to the child namespace.
  exec bwrap \
    --ro-bind / / --dev /dev --proc /proc \
    --tmpfs /tmp --tmpfs /run --setenv TMPDIR /tmp \
    --unshare-pid --unshare-net --unshare-ipc --unshare-uts \
    --die-with-parent --new-session \
    bash -c 'export LASH_LAUNCHER_TEST_NAMESPACE="$(readlink /proc/self/ns/mnt)"; exec bash "$@"' \
    bash "${BASH_SOURCE[1]}" "$@"
}
