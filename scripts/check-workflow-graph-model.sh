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
  crates/lash-protocol-rlm/src/executor/mod.rs; then
  echo "workflow graph model check failed: RLM no longer delegates its trace skeleton" >&2
  exit 1
fi

if ! grep -q 'workflow_graph_from_program' \
  crates/lash-lashlang-runtime/src/process/trace_map.rs; then
  echo "workflow graph model check failed: trace skeleton no longer projects WorkflowGraph" >&2
  exit 1
fi

# FIG-3571 (ADR 0100 R8): the IR crate and the process runtime stay
# language-neutral. Neither the `lashlang` library nor the
# `lash-lashlang-runtime` crate (library or tests) may reach a front-end crate
# through its dependency graph; a front end depends on them, never the reverse.
python3 - <<'PY'
import sys
import tomllib
from pathlib import Path

FRONT_END_PACKAGES = {"lash-internal-typescript"}
root = Path(".")
workspace = tomllib.loads((root / "Cargo.toml").read_text())
alias_to_path = {}
for alias, spec in workspace.get("workspace", {}).get("dependencies", {}).items():
    if isinstance(spec, dict) and "path" in spec:
        alias_to_path[alias] = spec["path"]


def manifest(path):
    return tomllib.loads((root / path / "Cargo.toml").read_text())


def package_name(path):
    return manifest(path)["package"]["name"]


def edges(path, tables):
    data = manifest(path)
    for table in tables:
        for alias, spec in data.get(table, {}).items():
            if isinstance(spec, dict) and spec.get("workspace"):
                target = alias_to_path.get(alias)
            elif isinstance(spec, dict) and "path" in spec:
                target = str((Path(path) / spec["path"]).resolve().relative_to(root.resolve()))
            else:
                target = None
            if target is not None:
                yield target


def reaches_front_end(start, tables):
    seen, stack = set(), [(start, tables, [package_name(start)])]
    while stack:
        path, used_tables, trail = stack.pop()
        for dependency in edges(path, used_tables):
            name = package_name(dependency)
            if name in FRONT_END_PACKAGES:
                return trail + [name]
            if dependency not in seen:
                seen.add(dependency)
                stack.append((dependency, ("dependencies",), trail + [name]))
    return None


failures = []
for crate, tables in (
    ("crates/lashlang", ("dependencies",)),
    ("crates/lash-lashlang-runtime", ("dependencies", "dev-dependencies")),
):
    trail = reaches_front_end(crate, tables)
    if trail:
        failures.append(" -> ".join(trail))
if failures:
    for trail in failures:
        print(f"workflow graph model check failed: language-neutral crate reaches a front end: {trail}", file=sys.stderr)
    sys.exit(1)
PY
