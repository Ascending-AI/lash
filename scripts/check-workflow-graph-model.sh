#!/usr/bin/env bash
set -euo pipefail

# Portable guards (grep, not rg): the Lint and functional-e2e runners do not
# ship ripgrep, so this uses GNU grep which is always present. `rg` under
# `set -e`+`pipefail` also propagated its command-not-found (127) out of the
# definition-count substitution, failing both jobs.

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

forbidden='static_graph_json|\bLashlangMap\b'
if grep -rnE --include='*.rs' "$forbidden" crates examples; then
  echo "workflow graph model check failed: retired definition-graph API still has consumers" >&2
  exit 1
fi

definition_count="$(grep -rnE --include='*.rs' '^pub struct WorkflowGraph \{' crates | wc -l | tr -d ' ')" || true
if [ "$definition_count" != "1" ]; then
  echo "workflow graph model check failed: expected one WorkflowGraph definition, found $definition_count" >&2
  exit 1
fi

if ! grep -qE 'lash_lashlang_runtime::trace_lashlang_main_map\(artifact\)' \
  crates/lash-protocol-rlm/src/executor.rs; then
  echo "workflow graph model check failed: RLM no longer delegates its trace skeleton" >&2
  exit 1
fi

if ! grep -q 'workflow_graph_from_program' crates/lash-lashlang-runtime/src/process.rs; then
  echo "workflow graph model check failed: trace skeleton no longer projects WorkflowGraph" >&2
  exit 1
fi
