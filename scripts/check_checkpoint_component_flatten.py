#!/usr/bin/env python3
"""Refuse `serde(flatten)` on types reachable from persisted checkpoint components.

ADR 0056 makes the checkpoint-component encoding a canonical typed
MessagePack surface: dynamic maps are sorted, struct fields are declared, and
the decode pre-pass rejects bytes whose field order does not match the
declaration. A flattened region has no fixed field order for that pre-pass to
validate, so a wire with reordered fields inside the flattened subtree would
be accepted and re-encode differently — the fixed-point invariant fails.

FIG-1210 removed the last component-reachable flatten (ToolDefinition inside
the RLM execution-state snapshot). This check keeps it removed: every file
whose serde carriers are wholly component-reachable is swept for `flatten`
inside `#[serde(...)]` attributes, and the component-reachable declarations
inside mixed-purpose files are checked symbol by symbol.

Flattens outside this inventory are not an escape hatch for persisted types —
they are the explicit boundary between durable component bytes and
transient/API surfaces. A new type that becomes reachable from a checkpoint
component must be enrolled here, and it must not flatten.
"""

from __future__ import annotations

import importlib.util
from pathlib import Path
import re
import sys

REPO_ROOT = Path(__file__).resolve().parent.parent
VERSION_BUMPS_SCRIPT = Path(__file__).with_name("check_version_bumps.py")

# Every serde carrier in these files belongs to a persisted checkpoint
# component body or to the checkpoint root manifest that lists them, so the
# whole file is swept.
WHOLE_FILES: tuple[str, ...] = (
    "crates/lash-core-store/src/store/checkpoint.rs",
    "crates/lash-core-store/src/tool_state.rs",
    "crates/lash-core-store/src/plugin_state.rs",
    "crates/lash-protocol-rlm/src/executor/state.rs",
    "crates/lash-lashlang-runtime/src/deferred.rs",
    "crates/lash-lashlang-runtime/src/deferred_triggers.rs",
    "crates/lash-protocol-rlm/src/projection/bindings.rs",
)

# These files hold both component-reachable carriers and types owned by other
# surfaces, so the component-reachable declarations are named one by one. The
# symbol lists cover the same serialized closures the versioned-surface guards
# name under SESSION_CHECKPOINT, TOOL_STATE/PLUGIN_STATE carriers, and
# RLM_SNAPSHOT_VERSION.
SYMBOL_FILES: dict[str, tuple[str, ...]] = {
    "crates/lash-core-store/src/session_graph.rs": ("PersistedTurnState",),
    "crates/lash-sansio/src/tool_contract.rs": (
        "ToolDefinition",
        "ToolManifest",
        "ToolContract",
        "ToolId",
        "CompactToolContract",
        "ToolActivation",
        "ToolRetryPolicy",
        "ToolOutputContract",
        "ToolArgumentProjectionPolicy",
    ),
    "crates/lash-sansio/src/schema_contract.rs": (
        "SchemaContract",
        "SchemaProjectionPolicy",
        "SchemaProjectionOverride",
        "ProjectionMode",
    ),
    "crates/lash-sansio/src/effect_identity.rs": (
        "EffectAddress",
        "ExecutionScope",
    ),
    "crates/lash-sansio/src/causal.rs": ("CausalRef",),
}

SERDE_ATTRIBUTE = re.compile(r"#\[\s*serde\s*\(")
FLATTEN_TOKEN = re.compile(r"\bflatten\b")


def load_named_rust_items():
    spec = importlib.util.spec_from_file_location(
        "check_version_bumps", VERSION_BUMPS_SCRIPT
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module.named_rust_items


def serde_attribute_spans(text: str) -> list[tuple[int, int]]:
    """Return the body span of each `#[serde(...)]` attribute.

    The body is everything between the attribute's opening and matching
    closing parenthesis; nested parentheses and string literals are skipped so
    options like `serde(with = "...")` do not truncate the span.
    """
    spans: list[tuple[int, int]] = []
    for match in SERDE_ATTRIBUTE.finditer(text):
        depth = 1
        cursor = match.end()
        in_string = False
        while cursor < len(text) and depth:
            char = text[cursor]
            if in_string:
                if char == "\\":
                    cursor += 2
                    continue
                if char == '"':
                    in_string = False
            elif char == '"':
                in_string = True
            elif char == "(":
                depth += 1
            elif char == ")":
                depth -= 1
            cursor += 1
        spans.append((match.end(), cursor - 1))
    return spans


def flatten_occurrences(text: str) -> list[int]:
    """Return byte offsets where `flatten` appears inside a serde attribute."""
    offsets: list[int] = []
    for start, end in serde_attribute_spans(text):
        for token in FLATTEN_TOKEN.finditer(text, start, end):
            offsets.append(token.start())
    return offsets


def line_number(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


def check_text(path: str, text: str, offsets: list[int]) -> list[str]:
    return [
        f"{path}:{line_number(text, offset)}: serde(flatten) on a type reachable "
        f"from a persisted checkpoint component (ADR 0056 forbids it; see FIG-1210)"
        for offset in offsets
    ]


def main() -> int:
    named_rust_items = load_named_rust_items()
    failures: list[str] = []
    for relative in WHOLE_FILES:
        path = REPO_ROOT / relative
        if not path.is_file():
            failures.append(f"{relative}: listed component file is missing")
            continue
        text = path.read_text()
        failures.extend(check_text(relative, text, flatten_occurrences(text)))
    for relative, symbols in SYMBOL_FILES.items():
        path = REPO_ROOT / relative
        if not path.is_file():
            failures.append(f"{relative}: listed component file is missing")
            continue
        text = path.read_text()
        items = named_rust_items(text, symbols)
        missing = set(symbols) - set(items)
        for symbol in sorted(missing):
            failures.append(f"{relative}: component-reachable symbol `{symbol}` is missing")
        for name, item in sorted(items.items()):
            if flatten_occurrences(item):
                failures.append(
                    f"{relative}: serde(flatten) on component-reachable `{name}` "
                    f"(ADR 0056 forbids it; see FIG-1210)"
                )
    if failures:
        print("checkpoint-component flatten check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print(
        f"checkpoint-component flatten check passed: "
        f"{len(WHOLE_FILES)} files, {sum(len(s) for s in SYMBOL_FILES.values())} symbols"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
