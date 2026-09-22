#!/usr/bin/env bash

# Sourced before a launcher test creates fixtures. The launcher keeps its
# real per-user ownership namespace; only the test's mount/PID/network view
# changes. Host PIDs, host network and the /run Docker socket are absent.
launcher_test_sandbox() {
  local namespace
  namespace="$(readlink /proc/self/ns/mnt)"
  # The environment value alone may be inherited from a caller. The witness
  # exists only in the private /tmp created by this launcher invocation.
  if [[ "${LASH_LAUNCHER_TEST_NAMESPACE:-}" == "$namespace" \
        && -f /tmp/.lash-launcher-test-namespace \
        && "$(</tmp/.lash-launcher-test-namespace)" == "$namespace" ]]; then
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
    --unsetenv DOCKER_HOST --unsetenv DOCKER_CONTEXT \
    --unshare-pid --unshare-net --unshare-ipc --unshare-uts \
    --die-with-parent --new-session \
    bash -c 'export LASH_LAUNCHER_TEST_NAMESPACE="$(readlink /proc/self/ns/mnt)"; printf "%s\n" "$LASH_LAUNCHER_TEST_NAMESPACE" > /tmp/.lash-launcher-test-namespace; exec bash "$@"' \
    bash "${BASH_SOURCE[1]}" "$@"
}
