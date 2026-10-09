#!/usr/bin/env python3
"""Require example and runbook hosts to consume Lash through its facade."""

from __future__ import annotations

from pathlib import Path
import re
import sys
import tomllib
from typing import Any, Iterator

REPO = Path(__file__).resolve().parents[1]


def dependency_tables(
    document: dict[str, Any],
    sections: frozenset[str] = frozenset({"dependencies", "build-dependencies"}),
) -> Iterator[dict[str, Any]]:
    for key, value in document.items():
        if key in sections and isinstance(value, dict):
            yield value
        elif isinstance(value, dict):
            yield from dependency_tables(value, sections)


def read_manifest(path: Path) -> dict[str, Any]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def forbidden_crates() -> set[str]:
    """The facade's first-party dependency closure, using Cargo's crate aliases."""
    manifests = {}
    for path in sorted((REPO / "crates").glob("*/Cargo.toml")):
        document = read_manifest(path)
        name = document.get("lib", {}).get("name", path.parent.name.replace("-", "_"))
        manifests[name] = document
    pending = [read_manifest(REPO / "crates/lash/Cargo.toml")]
    forbidden: set[str] = set()
    while pending:
        for dependencies in dependency_tables(pending.pop()):
            for alias in dependencies:
                crate = alias.replace("-", "_")
                # Workspace aliases retain the implementation crate name even
                # when the published package is named lash-internal-*.
                if crate in forbidden or not crate.startswith("lash_"):
                    continue
                forbidden.add(crate)
                if crate in manifests:
                    pending.append(manifests[crate])
    return forbidden


def source_manifest(source: Path) -> Path | None:
    for parent in source.parents:
        if parent == REPO:
            break
        manifest = parent / "Cargo.toml"
        if manifest.is_file():
            return manifest
    return None


def import_crates(source: Path, forbidden: set[str]) -> set[str]:
    """Include renamed dependencies so a Cargo alias cannot bypass the gate."""
    manifest = source_manifest(source)
    if manifest is None:
        return forbidden
    aliases = set(forbidden)
    for dependencies in dependency_tables(
        read_manifest(manifest),
        frozenset({"dependencies", "dev-dependencies", "build-dependencies"}),
    ):
        for alias, spec in dependencies.items():
            package = spec.get("package", alias) if isinstance(spec, dict) else alias
            package = package.removeprefix("lash-internal-")
            if package != alias:
                package = "lash-" + package if not package.startswith("lash-") else package
            if package.replace("-", "_") in forbidden:
                aliases.add(alias.replace("-", "_"))
    return aliases


def violations() -> list[tuple[Path, int, str]]:
    forbidden = forbidden_crates()
    sources = set((REPO / "examples").rglob("*.rs"))
    for root in (REPO / "runbooks").glob("*/src"):
        sources.update(root.rglob("*.rs"))
    found: list[tuple[Path, int, str]] = []
    for source in sorted(sources):
        relative = source.relative_to(REPO)
        names = "|".join(re.escape(name) for name in sorted(import_crates(source, forbidden)))
        if not names:
            continue
        pattern = re.compile(
            r"\b(?:" + names + r")\s*::|\b(?:use|extern\s+crate)\s+(?:"
            + names + r")\b(?!\s*::)"
        )
        for number, line in enumerate(source.read_text().splitlines(), 1):
            match = pattern.search(line)
            if match is not None:
                # Keep the rejected crate path as the diagnostic, including
                # imports written `use internal as alias` without a :: path.
                imported = re.sub(r"^(?:use|extern\s+crate)\s+", "", match.group(0))
                found.append((relative, number, imported))
    return found


def main() -> int:
    found = violations()
    if not found:
        print("example and runbook facade imports: no bypasses")
        return 0
    print("Example and runbook hosts must import API through the lash facade:", file=sys.stderr)
    for path, line, import_path in found:
        print(f"  {path}:{line}: {import_path}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
