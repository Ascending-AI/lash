#!/usr/bin/env python3
"""Refuse direct write stamps on migrate surfaces outside the fleet format.

ADR 0106 section 2's rollback-safety rule is that a durable writer emits the
version the fleet can read, not the newest its own build knows. FIG-3796 makes
the fleet-format row `F` the source of that prescription: for every
constant declared ``/// version_surface = "migrate"`` in source (the inventory
``discover_version_surfaces.py`` enforces) the writer stamps
``FleetFormat::writer_version(surface_format!(CONSTANT))``, never the bare
build constant. A tree with no such surface, or whose discovery fails, fails
the check: it would otherwise pass having checked nothing.

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


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))
from discover_version_surfaces import (  # noqa: E402
    RUST_SOURCE_PATTERNS, SourceError, WorktreeSources, _is_test_source, discover,
)

# A migrate surface whose stamp cannot consult `F`, with the reason.
EXEMPT_CONSTANTS = {
    # The store components stamp themselves: `F` lives inside the catalog
    # whose admission these versions gate, so a lookup could not precede the
    # write. Their window is the compat descriptor's range at admission
    # instead (ADR 0115 §1.3).
    "POSTGRES_SCHEMA_VERSION": "store schema version; the fleet row lives inside the catalog it admits",
    "SQLITE_CORE_SCHEMA_VERSION": "store schema version; the fleet row lives inside the catalog it admits",
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

# These tables describe the fleet's compatibility policy. Their version
# fields are inputs to writer_version, never stamps on stored records.
FLEET_POLICY_TABLE = re.compile(
    r"\b(?:pub\s+)?const\s+(?:RECORD_UPCASTERS|WRITER_PINS)\s*:[^\n]*=\s*&\["
)

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


def fleet_policy_ranges(text: str) -> list[tuple[int, int]]:
    return [
        (match.start(), text.find("];", match.end()) + 2)
        for match in FLEET_POLICY_TABLE.finditer(text)
    ]


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


def check(sources: WorktreeSources, constants: list[str]) -> list[str]:
    patterns = {constant: stamp_pattern(constant) for constant in constants}
    failures: list[str] = []
    for relative in sources.matching_paths(RUST_SOURCE_PATTERNS):
        text = sources.content(relative)
        if text is None or _is_test_source(relative):
            continue
        skipped = test_item_ranges(text)
        if relative == "crates/lash-core-store/src/store/fleet_format.rs":
            skipped.extend(fleet_policy_ranges(text))
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


def migrate_constants(sources: WorktreeSources) -> list[str]:
    """Source-declared migrate surfaces, one per constant name.

    Raises SourceError when discovery cannot establish the inventory.
    """
    surfaces, _ = discover(sources)
    return sorted({
        surface["constant"] for surface in surfaces
        if surface["upgrade"] == "migrate" and surface["constant"] not in EXEMPT_CONSTANTS
    })


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=ROOT, help=argparse.SUPPRESS)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(list(argv) if argv is not None else sys.argv[1:])
    sources = WorktreeSources(args.repo)
    try:
        constants = migrate_constants(sources)
    except SourceError as error:
        print(f"fleet-format writer-stamp check cannot discover surfaces:\n{error}", file=sys.stderr)
        return 1
    if not constants:
        print(
            "fleet-format writer-stamp check found no source-declared migrate surface "
            "to check; refusing to pass vacuously",
            file=sys.stderr,
        )
        return 1
    failures = check(sources, constants)
    if failures:
        print("fleet-format writer-stamp violations:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print(f"fleet-format writer stamps checked for {len(constants)} migrate surfaces")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
