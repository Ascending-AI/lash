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

if ! grep -q 'lashlang::workflow_graph_from_artifact' \
  crates/lash-lashlang-runtime/src/process/trace_map.rs; then
  echo "workflow graph model check failed: trace skeleton no longer projects the admitted artifact's WorkflowGraph" >&2
  exit 1
fi

# FIG-3571 (ADR 0100 R8): the IR crate and the process runtime stay
# language-neutral. Each workspace crate declares its role in
# `[package.metadata.lash] role`: a `language-neutral` crate may not reach a
# `front-end` crate through its dependency graph (dev-dependencies included),
# and a front end depends on it, never the reverse. The one recorded
# exemption is a dev-dependency edge, listed below with its reason.
python3 - <<'PY'
import sys
import tomllib
from pathlib import Path

# (language-neutral package, front-end package) -> why the dev-dependency is
# admitted. A normal dependency is never exempt.
DEV_DEPENDENCY_EXEMPTIONS = {
    ("lash-internal-lashlang", "lash-internal-typescript"): (
        "lashlang's integration tests and benches author their programs in "
        "TypeScript; the library and its unit tests never link a front end"
    ),
}

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


def role(path):
    return manifest(path)["package"].get("metadata", {}).get("lash", {}).get("role")


members = [member for member in workspace["workspace"]["members"] if (root / member / "Cargo.toml").exists()]
front_ends = {package_name(member) for member in members if role(member) == "front-end"}
neutral = [member for member in members if role(member) == "language-neutral"]
if not front_ends or not neutral:
    print("workflow graph model check failed: no crate declares a front-end or language-neutral role", file=sys.stderr)
    sys.exit(1)


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
                yield table, target


def front_end_trails(start):
    trails, seen = [], set()
    stack = [(start, ("dependencies", "dev-dependencies"), [package_name(start)], None)]
    while stack:
        path, tables, trail, first_table = stack.pop()
        for table, dependency in edges(path, tables):
            name = package_name(dependency)
            first = first_table or table
            if name in front_ends:
                trails.append((first, trail + [name]))
                continue
            if (dependency, first) not in seen:
                seen.add((dependency, first))
                stack.append((dependency, ("dependencies",), trail + [name], first))
    return trails


failures = []
for crate in neutral:
    for first_table, trail in front_end_trails(crate):
        exempt = (
            first_table == "dev-dependencies"
            and (trail[0], trail[-1]) in DEV_DEPENDENCY_EXEMPTIONS
        )
        if not exempt:
            failures.append(f"{' -> '.join(trail)} (through {first_table})")
if failures:
    for trail in failures:
        print(f"workflow graph model check failed: language-neutral crate reaches a front end: {trail}", file=sys.stderr)
    sys.exit(1)
PY

# FIG-3571: structure is read from IR forms and structural roles, never from a
# binding's spelling. Every crate and example is scanned: code may compare a
# builtin against the declared opcode vocabulary by exact name, but it may not
# recognise a shape by a generated-name prefix. Each remaining use is listed
# below with the reason it is not structure recognition.
python3 - <<'PY'
import re
import sys
from pathlib import Path

RECOGNITION = re.compile(
    r"\.(starts_with|ends_with|strip_prefix|strip_suffix)\(\s*\"__"
    r"|GENERATED_BINDING_PREFIX|LIFTED_PROCESS_NAME_PREFIX|\"__process_"
)
# path (a file, or a directory with a trailing slash) -> reason.
ALLOWED = {
    "crates/lash-typescript/src/lower/": (
        "the TypeScript lowerer mints its generated names and reserves the "
        "prefixes against authored identifiers"
    ),
    "crates/lash-typescript/src/lib.rs": "re-exports the lowerer's prefix to the printer",
    "crates/lash-typescript/src/workflow_graph/printer.rs": (
        "the printer refuses to spell a generated binding as source"
    ),
    "crates/lash-typescript/src/workflow_graph/mod.rs": (
        "graph validation refuses a process whose name contradicts its origin"
    ),
    "crates/lashlang/src/ast_roles.rs": (
        "defines the lifted-name prefix and validates names against origins"
    ),
    "crates/lashlang/src/ast.rs": "re-exports the lifted-name prefix",
    "crates/lashlang/src/lib.rs": "re-exports the lifted-name prefix",
    "crates/lash-protocol-rlm/src/protocol/prompt.rs": (
        "hides the reserved `__` tool and module namespace from the model "
        "(ADR 0096), not a binding"
    ),
    "crates/lash-perf/src/runtime_perf/providers/tools.rs": (
        "decodes the perf harness's own tool-name scheme"
    ),
    "examples/toolbench/src/runtime.rs": "classifies the toolbench's own task ids",
}


def is_test(path):
    parts = path.parts
    return (
        "tests" in parts
        or "testing" in parts
        or "benches" in parts
        or path.name.endswith("_tests.rs")
        or path.name == "tests.rs"
    )


def allowed(path):
    text = path.as_posix()
    return any(
        text.startswith(entry) if entry.endswith("/") else text == entry for entry in ALLOWED
    )


failures = []
for base in (Path("crates"), Path("examples")):
    for path in sorted(base.rglob("*.rs")):
        if "target" in path.parts or "node_modules" in path.parts or is_test(path) or allowed(path):
            continue
        for number, line in enumerate(path.read_text().splitlines(), start=1):
            if RECOGNITION.search(line):
                failures.append(f"{path}:{number}: {line.strip()}")
if failures:
    print("workflow graph model check failed: structure recognised by a name:", file=sys.stderr)
    print("\n".join(failures), file=sys.stderr)
    sys.exit(1)
PY
