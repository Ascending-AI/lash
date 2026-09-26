#!/usr/bin/env python3
"""Refuse direct write stamps on migrate surfaces outside the fleet format.

ADR 0106 section 2's rollback-safety rule is that a durable writer emits the
version the fleet can read, not the newest its own build knows. FIG-3796 makes
the fleet-format row `F` the source of that prescription: for every
``upgrade = "migrate"`` surface in ``scripts/versioned-surfaces.toml`` the
writer stamps ``FleetFormat::writer_version(surface_format!(CONSTANT))``,
never the bare build constant.

The regression shape this check refuses is the constant in an initialiser
position — ``version: CONSTANT``, ``schema_version: CONSTANT``,
``encoding_version: CONSTANT``, or the quoted ``"version": CONSTANT`` a
``json!`` literal emits. Everything else the constant does is not a write
stamp: read-side gates (``!=``, ``==``, ``expected:``, ``ensure_supported_*``),
diagnostics and remedy text, its own definition, re-exports, and uses inside
test modules are ignored. A ``#[cfg(...test...)]`` item's body is skipped as
well: the testing facades mint build-current stamps for non-durable callers
by design.

The check reads sources only and uses the standard library alone, so it can
run before the Rust toolchain is installed.
"""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_format_registry  # noqa: E402

DEFAULT_REGISTRY = Path(__file__).with_name("versioned-surfaces.toml")
SWEPT_ROOTS = check_format_registry.SWEPT_ROOTS

# A migrate surface whose stamp cannot consult `F`, with the reason.
EXEMPT_CONSTANTS = {
    # The catalog components stamp themselves: `F` lives inside the catalog
    # whose admission these stamps gate, so a lookup could not precede the
    # write. Their writable window is FIG-3797's
    # [MIN_SUPPORTED, SCHEMA_VERSION] range check at admission instead.
    "SCHEMA_VERSION": "catalog DDL stamp; the fleet row lives inside the catalog it admits",
    "PROCESS_SCHEMA_VERSION": "catalog DDL stamp; the fleet row lives inside the catalog it admits",
    "TRIGGER_SCHEMA_VERSION": "catalog DDL stamp; the fleet row lives inside the catalog it admits",
    "EFFECT_SCHEMA_VERSION": "catalog DDL stamp; the fleet row lives inside the catalog it admits",
    # The row every other writer consults has nothing upstream of it.
    "FLEET_FORMAT_VERSION": "the fleet-format row is what writers consult",
}

# (path suffix, constant or "*"): write-stamp-shaped uses that are not
# durable write stamps, each with the reason a reviewer needs.
EXEMPTIONS: list[tuple[str, str, str]] = [
    (
        "crates/lash/src/formats.rs",
        "*",
        "the format manifest names every constant in its table",
    ),
    (
        "crates/lash-restate/src/formats.rs",
        "*",
        "the engine's durable-format manifest names every constant in its "
        "table; the formats' bytes live in the Restate deployment `F` does "
        "not govern",
    ),
    (
        "crates/lash-core-store/src/protocol_turn_options.rs",
        "PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION",
        "empty()/from_payload mint the value-carried stamp at the build "
        "default for non-durable callers; durable boundaries restamp with "
        "restamped_for_fleet",
    ),
    (
        "crates/lashlang/src/runtime/state.rs",
        "LASHLANG_SNAPSHOT_VERSION",
        "CanonicalSnapshot::try_from seeds the wire header at the build "
        "default; to_canonical_bytes_at_version restamps it at the durable "
        "boundary",
    ),
    (
        "crates/lashlang/src/runtime/heap.rs",
        "HEAP_SIZE_SCHEDULE_VERSION",
        "Heap::default mints the heap's own schedule stamp at birth; durable "
        "wires carry the heap's recorded value",
    ),
    (
        "crates/lash/src/preflight/",
        "*",
        "read-side extraction and probe reports, not durable writes",
    ),
    (
        "examples/",
        "*",
        "examples have no store and no fleet to consult",
    ),
    (
        "_schema_generator.rs",
        "*",
        "schema-document generators emit the constant into generated docs",
    ),
]

# `#[cfg(...test...)]` decorating any item body: mod, fn, impl.
TEST_ATTRIBUTE = re.compile(r"#\[cfg\([^]]*\btest\b[^]]*\)\]")

# A constant used in initialiser position: `version: CONST`,
# `schema_version: CONST`, `"version": CONST`, `version: path::to::CONST`.
def stamp_pattern(constant: str) -> re.Pattern[str]:
    return re.compile(
        r"\b\w*(?:version|format)\"?\s*:\s*(?:[a-z_][\w]*::)*" + constant + r"\b"
    )


def test_item_ranges(text: str) -> list[tuple[int, int]]:
    """Bodies of items a ``#[cfg(...test...)]`` decorates."""
    ranges: list[tuple[int, int]] = []
    for match in TEST_ATTRIBUTE.finditer(text):
        # The decorated item's body starts at its first `{`; a `;`, another
        # attribute, or a `where`-less prototype means there is no body to
        # skip (the attribute still does not cover a later write stamp).
        cursor = match.end()
        start = None
        while cursor < len(text) and cursor < match.end() + 4096:
            char = text[cursor]
            if char == "{":
                start = cursor
                break
            if char == ";" or text.startswith("#[", cursor):
                break
            cursor += 1
        if start is None:
            continue
        depth = 1
        index = start + 1
        while index < len(text) and depth:
            if text[index] == "{":
                depth += 1
            elif text[index] == "}":
                depth -= 1
            index += 1
        ranges.append((match.start(), index))
    return ranges


def exempt(path: str, constant: str) -> str | None:
    for suffix, exempt_constant, reason in EXEMPTIONS:
        if exempt_constant not in ("*", constant):
            continue
        if suffix.endswith("/"):
            if path.startswith(suffix) or f"/{suffix}" in f"/{path}":
                return reason
        elif path.endswith(suffix):
            return reason
    return None


def check(repo: Path, constants: list[str]) -> list[str]:
    patterns = {constant: stamp_pattern(constant) for constant in constants}
    failures: list[str] = []
    for root in SWEPT_ROOTS:
        base = repo / root
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*.rs")):
            relative = path.relative_to(repo).as_posix()
            if "/target/" in f"/{relative}" or check_format_registry.is_test_path(relative):
                continue
            text = path.read_text(encoding="utf-8", errors="surrogateescape")
            skipped = test_item_ranges(text)
            for constant, pattern in patterns.items():
                for match in pattern.finditer(text):
                    if any(start <= match.start() < end for start, end in skipped):
                        continue
                    reason = exempt(relative, constant)
                    if reason is not None:
                        continue
                    line = text.count("\n", 0, match.start()) + 1
                    failures.append(
                        f"{relative}:{line}: `{constant}` used directly in a "
                        f"write-stamp position; stamp "
                        f"FleetFormat::writer_version(surface_format!({constant})) "
                        f"instead"
                    )
    return failures


def load_migrate_constants(path: Path) -> list[str]:
    document = tomllib.loads(path.read_text(encoding="utf-8"))
    constants = []
    for surface in document.get("surface", []):
        constant = surface.get("constant")
        if surface.get("upgrade") == "migrate" and constant not in EXEMPT_CONSTANTS:
            constants.append(constant)
    return sorted(constants)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=ROOT, help=argparse.SUPPRESS)
    parser.add_argument(
        "--registry", type=Path, default=DEFAULT_REGISTRY, help=argparse.SUPPRESS
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(list(argv) if argv is not None else sys.argv[1:])
    failures = check(args.repo, load_migrate_constants(args.registry))
    if failures:
        print("fleet-format writer-stamp violations:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
