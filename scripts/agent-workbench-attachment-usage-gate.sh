#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# The port argument stays accepted and validated for compatibility with
# `just agent-workbench-attachment-usage-gate <port>`; the SQLite pass does not
# bind it.
workbench_port="${1:-3030}"
if [[ ! "$workbench_port" =~ ^[0-9]+$ ]]; then
  printf 'workbench port must be numeric, got %s\n' "$workbench_port" >&2
  exit 2
fi
workbench_port_number=$((10#$workbench_port))
if (( workbench_port_number < 1 || workbench_port_number > 65535 )); then
  printf 'workbench port must be between 1 and 65535, got %s\n' "$workbench_port" >&2
  exit 2
fi

printf '[attachment-usage-gate] SQLite file-store/session-store pass\n'
kiln run //examples/agent-workbench:agent-workbench__unit_test -- \
  attachment_usage_gate_sqlite --nocapture --test-threads=1

printf '[attachment-usage-gate] upload -> reference -> persist -> retrieve and usage restart gates passed\n'
