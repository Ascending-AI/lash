#!/usr/bin/env python3
"""Hold the durable format-version registry exhaustive and the manifest true.

``scripts/versioned-surfaces.toml`` is the one registry of versioned formats.
Every ``*_VERSION`` / ``*_EPOCH`` constant defined in non-test Rust under
``crates/`` and ``examples/`` must be one of:

- a registered ``[[surface]]`` (bump-guarded by ``check_version_bumps.py``);
- a member of a ``[[excluded_class]]`` suffix, whose reason is written once
  for the whole class; or
- an ``[[unregistered]]`` entry naming the constant and why it versions no
  durable format.

An exclusion that no longer names a swept constant fails too, so the list
cannot rot into a blanket allowance.

The format manifest in ``crates/lash/src/formats.rs`` is checked against the
same registry, in both directions: every registered surface either names its
manifest row (``manifest = "<DurableFormat variant>"``), states why it is
outside the manifest (``outside_manifest = "<reason>"``), or belongs to an
excluded class; and every manifest row is exactly one registered surface whose
``manifest`` names it. That is what makes the manifest's exhaustiveness claim
checkable rather than aspirational.

Only the Python standard library is used.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import re
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CONFIG = Path(__file__).with_name("versioned-surfaces.toml")
MANIFEST = Path("crates/lash/src/formats.rs")
SWEPT_ROOTS = ("crates", "examples")

VERSION_CONSTANT = re.compile(
    r"^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?const[ \t]+"
    r"(?P<name>[A-Z][A-Z0-9_]*(?:_VERSION|_EPOCH))[ \t]*:",
    re.MULTILINE,
)
MANIFEST_ROW = re.compile(
    r"format:\s*DurableFormat::(?P<variant>\w+),\s*"
    r"version:\s*FormatVersion::\w+\((?P<symbol>\w+)(?:\s+as\s+u32)?\),\s*"
    r"owning_crate:\s*\"[^\"]+\",\s*"
    r"constant:\s*\"(?P<constant>\w+)\""
)
MANIFEST_ENTRY = re.compile(r"\bDurableFormatEntry\s*\{")
TEST_ATTRIBUTE = re.compile(r"#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")


class RegistryError(Exception):
    """The registry or manifest cannot be read."""


@dataclass(frozen=True)
class Registry:
    surfaces: dict[str, dict]
    classes: dict[str, str]
    unregistered: dict[str, str]


def is_test_path(relative: str) -> bool:
    parts = relative.split("/")
    name = parts[-1]
    return (
        "tests" in parts[:-1]
        or "benches" in parts[:-1]
        or name in {"tests.rs", "test.rs"}
        or name.endswith("_tests.rs")
    )


def test_module_ranges(text: str) -> list[tuple[int, int]]:
    """Bodies of inline ``#[cfg(test)] mod name { ... }`` modules."""
    ranges: list[tuple[int, int]] = []
    for match in TEST_ATTRIBUTE.finditer(text):
        depth = 1
        index = match.end()
        while index < len(text) and depth:
            if text[index] == "{":
                depth += 1
            elif text[index] == "}":
                depth -= 1
            index += 1
        ranges.append((match.end(), index))
    return ranges


def sweep(repo: Path) -> set[str]:
    """Every non-test ``path:CONSTANT`` key whose name is version-shaped."""
    keys: set[str] = set()
    for root in SWEPT_ROOTS:
        base = repo / root
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*.rs")):
            relative = path.relative_to(repo).as_posix()
            if "/target/" in f"/{relative}" or is_test_path(relative):
                continue
            text = path.read_text(encoding="utf-8", errors="surrogateescape")
            excluded = test_module_ranges(text)
            for match in VERSION_CONSTANT.finditer(text):
                if any(start <= match.start() < end for start, end in excluded):
                    continue
                keys.add(f"{relative}:{match.group('name')}")
    return keys


def _reason(raw: dict, field: str, location: str) -> str:
    value = raw.get(field)
    if not isinstance(value, str) or not value.strip():
        raise RegistryError(f"{location} needs a non-empty {field}")
    return value


def load_registry(path: Path) -> Registry:
    try:
        document = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise RegistryError(f"cannot read {path}: {error}") from error

    surfaces: dict[str, dict] = {}
    for index, raw in enumerate(document.get("surface", []), start=1):
        key = f"{raw.get('constant_path')}:{raw.get('constant')}"
        surfaces[key] = raw
    if not surfaces:
        raise RegistryError(f"{path}: no [[surface]] entries")

    classes: dict[str, str] = {}
    for index, raw in enumerate(document.get("excluded_class", []), start=1):
        location = f"{path}: excluded_class {index}"
        suffix = raw.get("suffix")
        if not isinstance(suffix, str) or not suffix:
            raise RegistryError(f"{location} needs a suffix")
        classes[suffix] = _reason(raw, "reason", location)

    unregistered: dict[str, str] = {}
    for index, raw in enumerate(document.get("unregistered", []), start=1):
        location = f"{path}: unregistered {index}"
        constant = raw.get("constant")
        constant_path = raw.get("constant_path")
        if not isinstance(constant, str) or not isinstance(constant_path, str):
            raise RegistryError(f"{location} needs constant and constant_path")
        key = f"{constant_path}:{constant}"
        if key in unregistered:
            raise RegistryError(f"{location} duplicates {key}")
        unregistered[key] = _reason(raw, "reason", location)
    return Registry(surfaces, classes, unregistered)


def class_of(registry: Registry, key: str) -> str | None:
    constant = key.rsplit(":", 1)[1]
    for suffix in registry.classes:
        if constant.endswith(suffix):
            return suffix
    return None


def manifest_rows(text: str) -> dict[str, str]:
    """``constant -> DurableFormat variant`` for every manifest row."""
    rows: dict[str, str] = {}
    entries = len(MANIFEST_ENTRY.findall(text)) - 1  # minus the struct itself
    matches = list(MANIFEST_ROW.finditer(text))
    if len(matches) != entries:
        raise RegistryError(
            f"{MANIFEST}: parsed {len(matches)} rows but found {entries} "
            "DurableFormatEntry literals; keep each row's fields in the order "
            "format, version, owning_crate, constant"
        )
    for match in matches:
        constant = match.group("constant")
        if match.group("symbol") != constant:
            raise RegistryError(
                f"{MANIFEST}: row {match.group('variant')} reports "
                f"{match.group('symbol')} but names constant {constant}"
            )
        if constant in rows:
            raise RegistryError(f"{MANIFEST}: {constant} is listed twice")
        rows[constant] = match.group("variant")
    return rows


def check(repo: Path, registry: Registry, manifest_text: str) -> list[str]:
    problems: list[str] = []

    swept = sweep(repo)
    for key in sorted(swept):
        if key in registry.surfaces or key in registry.unregistered:
            continue
        if class_of(registry, key) is not None:
            continue
        problems.append(
            f"{key} is a version-shaped constant the registry does not know: "
            "register it as a [[surface]] in scripts/versioned-surfaces.toml, or "
            "add an [[unregistered]] entry stating why it versions no durable format"
        )
    for key in sorted(registry.unregistered):
        if key not in swept:
            problems.append(
                f"[[unregistered]] {key} names no swept constant; delete the stale "
                "exclusion"
            )
        elif key in registry.surfaces:
            problems.append(f"{key} is both a registered surface and [[unregistered]]")

    rows = manifest_rows(manifest_text)
    claimed: dict[str, str] = {}
    for key, raw in sorted(registry.surfaces.items()):
        constant = raw.get("constant")
        manifest = raw.get("manifest")
        outside = raw.get("outside_manifest")
        in_class = class_of(registry, key) is not None
        declared = [
            label
            for label, present in (
                ("manifest", manifest is not None),
                ("outside_manifest", outside is not None),
                ("an excluded class", in_class),
            )
            if present
        ]
        if len(declared) != 1:
            detail = ", ".join(declared) if declared else "none"
            problems.append(
                f"{key} must have exactly one manifest disposition (manifest, "
                f"outside_manifest, or an excluded class); has {detail}"
            )
            continue
        if outside is not None and (not isinstance(outside, str) or not outside.strip()):
            problems.append(f"{key} outside_manifest must state a reason")
        if manifest is None:
            continue
        if not isinstance(manifest, str) or not manifest:
            problems.append(f"{key} manifest must name a DurableFormat variant")
            continue
        if constant in claimed:
            problems.append(
                f"{key}: {constant} is claimed by more than one surface "
                f"({claimed[constant]}); manifest rows are keyed by constant name"
            )
            continue
        claimed[constant] = key
        row = rows.get(constant)
        if row is None:
            problems.append(
                f"{key} declares manifest = {manifest!r} but {MANIFEST} has no row "
                f"reporting {constant}"
            )
        elif row != manifest:
            problems.append(
                f"{key} declares manifest = {manifest!r} but {MANIFEST} reports "
                f"{constant} as DurableFormat::{row}"
            )
    for constant, variant in sorted(rows.items()):
        if constant not in claimed:
            problems.append(
                f"{MANIFEST} row DurableFormat::{variant} reports {constant}, which "
                f"no registered surface claims with manifest = {variant!r}"
            )
    return problems


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--repo", type=Path, default=ROOT, help=argparse.SUPPRESS)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        registry = load_registry(args.config)
        manifest_text = (args.repo / MANIFEST).read_text(encoding="utf-8")
        problems = check(args.repo, registry, manifest_text)
    except (OSError, RegistryError) as error:
        print(f"format-registry check error: {error}", file=sys.stderr)
        return 2
    if problems:
        print("format-registry check failed:", file=sys.stderr)
        for problem in problems:
            print(f"- {problem}", file=sys.stderr)
        return 1
    print(
        f"format-registry check passed: {len(registry.surfaces)} surfaces, "
        f"{len(registry.unregistered)} stated exclusions, "
        f"{len(registry.classes)} excluded classes"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
