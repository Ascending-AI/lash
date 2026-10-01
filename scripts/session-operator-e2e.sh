#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
: "${KILN_GATE_ID:?run through kiln gate lash <fork> -- just session-operator-e2e}"
gate_sum="$(printf '%s' "$KILN_GATE_ID" | cksum)"
export LASH_GATE_SLOT_OVERRIDE="$(( ${gate_sum%% *} % 90 ))"
source scripts/worktree-gate-env.sh
lash_gate_acquire_locks session-operator-e2e
trap lash_gate_cleanup EXIT
if [[ -n "${LASH_OPERATOR_ARTIFACT_ROOT:-}" ]]; then
  artifacts="$LASH_OPERATOR_ARTIFACT_ROOT"
else
  mkdir -p "$PWD/target/session-operator/$KILN_GATE_ID"
  artifacts="$(mktemp -d "$PWD/target/session-operator/$KILN_GATE_ID/campaign.XXXXXX")"
fi
mkdir -p "$artifacts"
echo "session operator artifacts: $artifacts"
mapfile -t built < <(python3 scripts/ci/restate_suite.py build \
  //runbooks/restate-postgres-workers:lash-e2e-session-operator__bin \
  //crates/lash-vm-worker:lash-vm-worker__bin)
[[ "${#built[@]}" = 2 ]] || { echo 'expected operator and VM worker binaries' >&2; exit 1; }
export LASH_OPERATOR_VM_WORKER="${built[1]}"
python3 scripts/session_operator_e2e.py \
  "${built[0]}" "$artifacts" "$LASH_E2E_PORT_BASE" "${LASH_OPERATOR_RUNS:-20}"
