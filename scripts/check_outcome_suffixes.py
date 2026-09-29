#!/usr/bin/env python3
"""ADR 0068 outcome-suffix gate.

A public type a host can name must not end in `Result`, `Disposition`, or
`Summary`. `Result` is reserved for `std::Result` aliases; `Disposition` and
`Summary` are retired outright (a projection is a `...View` or the domain
noun, an aggregate over many items is a `...Report`).

The scanned surface is what the ADR's wave moved:

* the facade's re-exports in `crates/lash/src/lib.rs`, plus every `pub` item
  defined under `crates/lash/src/` (a `pub` item in a `pub mod` is nameable
  whether or not the root re-exports it), and
* the `lash-remote-protocol` public surface: every `pub` item under
  `crates/lash-remote-protocol/src/`.

`pub use ...::*` re-exports in the facade resolve into the target crate's
(or module's) `pub` items, so a feature-gated glob like `lash_restate::*`
cannot smuggle a retired suffix past the gate.
"""

from __future__ import annotations

from pathlib import Path
import re
import sys

REPO = Path(__file__).resolve().parents[1]
FACADE = REPO / "crates/lash"
REMOTE = REPO / "crates/lash-remote-protocol"

RETIRED = ("Result", "Disposition", "Summary")

ITEM = re.compile(r"(?m)^pub (?:struct|enum|union|trait|type) ([A-Za-z_][A-Za-z0-9_]*)")
PUB_USE = re.compile(r"pub use\s+([^;]+);", re.DOTALL)
IDENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")

# Genuine `std::Result` aliases — the one use of `Result` ADR 0068 reserves.
# A name lands here only by ending in `= ... Result<...>` at its definition;
# domain types never do.
RESULT_ALIASES = frozenset(
    {
        "Result",
        "MaintenanceResult",
        "TriggerEffectResult",
        "TriggerOccurrenceReclamationResult",
    }
)

# Retired-suffix types still nameable on the surface, owned by audit tickets
# outside FIG-4107's three renames. Shrinks only: a rename lands by removing
# the entry, a new name never joins.
PENDING = frozenset(
    {
        "AttemptUsageDisposition",  # sans-io attempt usage outcome; pending rename
        "CodeExecutionDisposition",  # code-effect settlement; pending rename
        "EffectGroupCloseDisposition",  # restate group-close policy; pending
        "ParkSummary",  # live-park aggregate; pending rename
        "ParkedWorkSummary",  # parked-work aggregate; pending rename
        "ProcessEffectNodeSummary",  # per-node effect aggregate; pending
        "ProcessEffectSummary",  # effect-log aggregate; pending rename
        "RemoteAttemptUsageDisposition",  # wire mirror of the pending core type
        "RemoteProcessEffectNodeSummary",  # wire mirror of the pending core type
        "RemoteTurnCancelDisposition",  # wire mirror of the pending core type
        "StoppedPartialSummary",  # stopped-partial projection; pending rename
        "TraceAttemptUsageDisposition",  # trace mirror of the pending core type
        "TraceLashlangNodeSummary",  # trace node projection; pending rename
        "TraceLashlangNodeTerminalSummary",  # trace terminal projection; pending
        "TurnCancelDisposition",  # cancel-honour timing policy; pending rename
        "WakeDeliveryDisposition",  # wake delivery state machine; pending rename
    }
)


def rust_sources(crate: Path) -> list[Path]:
    return sorted(crate.glob("src/**/*.rs"))


def public_items(files: list[Path]) -> list[tuple[Path, int, str]]:
    """Every `pub` item name declared in the sources, with its origin."""
    found = []
    for source in files:
        text = source.read_text()
        for match in ITEM.finditer(text):
            line = text.count("\n", 0, match.start()) + 1
            found.append((source.relative_to(REPO), line, match.group(1)))
    return found


def use_tree_leaves(tree: str) -> list[str]:
    """Leaf names of one `use` tree: `a::{b::C, D as E}` -> [C, E]."""
    tree = " ".join(tree.split())
    if "{" in tree:
        head, _, rest = tree.partition("{")
        if not rest.endswith("}") or "{" in rest:
            raise ValueError(f"nested or malformed use tree: {tree!r}")
        leaves = []
        for item in rest[:-1].split(","):
            item = item.strip()
            if not item or item == "self":
                continue
            leaves.extend(use_tree_leaves(f"{head}::{item}"))
        return leaves
    if tree.endswith("::*"):
        return []
    leaf = tree.rsplit("::", 1)[-1]
    if " as " in leaf:
        leaf = leaf.rsplit(" as ", 1)[-1].strip()
    return [leaf] if IDENT.match(leaf) else []


def glob_target_sources(path: str) -> list[Path]:
    """Sources behind `pub use <path>::*` in the facade root."""
    parts = [segment.strip() for segment in path.split("::")]
    if parts[0] == "crate":
        base = FACADE / "src"
        parts = parts[1:]
    elif parts[0].startswith("lash_") or parts[0].startswith("lash-"):
        base = REPO / "crates" / parts[0].replace("_", "-") / "src"
        parts = parts[1:]
    else:
        return []
    for parent in parts[:-1]:
        base = base / parent
    if not parts:
        return sorted(base.rglob("*.rs")) if base.is_dir() else []
    files = []
    module = base / parts[-1]
    module_file = base / f"{parts[-1]}.rs"
    if module.is_dir():
        files.extend(sorted(module.rglob("*.rs")))
    if module_file.is_file():
        files.append(module_file)
    return files


def facade_exports(lib: Path) -> list[tuple[Path, int, str]]:
    """Identifiers a host can name through the facade's `pub use` surface."""
    text = lib.read_text()
    found = []
    for match in PUB_USE.finditer(text):
        tree = match.group(1)
        line = text.count("\n", 0, match.start()) + 1
        if tree.rstrip().endswith("::*"):
            for source, item_line, name in public_items(glob_target_sources(tree[:-3])):
                found.append((source, item_line, name))
            continue
        try:
            for name in use_tree_leaves(tree):
                found.append((lib.relative_to(REPO), line, name))
        except ValueError:
            found.append((lib.relative_to(REPO), line, f"<unparsed: {tree[:60]}>"))
    return found


def main() -> int:
    candidates = facade_exports(FACADE / "src/lib.rs")
    candidates.extend(public_items(rust_sources(FACADE)))
    candidates.extend(public_items(rust_sources(REMOTE)))

    violations = {}
    for source, line, name in candidates:
        if name in RESULT_ALIASES or name in PENDING:
            continue
        if name.endswith(RETIRED):
            violations.setdefault(name, []).append(f"{source}:{line}")

    if not violations:
        print("outcome suffixes: no retired-suffix type is host-nameable")
        return 0
    print(
        "ADR 0068: no host-nameable type ends in Result, Disposition, or Summary:",
        file=sys.stderr,
    )
    for name in sorted(violations):
        print(f"  {name}", file=sys.stderr)
        for site in violations[name]:
            print(f"    {site}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
