#!/usr/bin/env python3
"""Require examples to consume Lash through its host-facing facade."""

from __future__ import annotations

from pathlib import Path
import re
import sys
import tomllib
from typing import Any, Iterator


REPO = Path(__file__).resolve().parents[1]
EXAMPLES = REPO / "examples"
ALWAYS_FORBIDDEN = re.compile(r"\b(?:lash_core|lash_sansio|lash_internal)::")
LASHLANG = re.compile(r"\blashlang::")


def dependency_tables(document: dict[str, Any]) -> Iterator[dict[str, Any]]:
    for key, value in document.items():
        if key in {"dependencies", "dev-dependencies", "build-dependencies"}:
            if isinstance(value, dict):
                yield value
        elif isinstance(value, dict):
            yield from dependency_tables(value)


def dependency_features(spec: Any) -> set[str]:
    if not isinstance(spec, dict):
        return set()
    features = spec.get("features", [])
    return set(features) if isinstance(features, list) else set()


def is_lashlang_context(manifest: Path) -> bool:
    """Whether this example deliberately embeds the RLM/Lashlang surface."""
    with manifest.open("rb") as handle:
        document = tomllib.load(handle)
    for dependencies in dependency_tables(document):
        if "rlm" in dependency_features(dependencies.get("lash")):
            return True
        # A Lashlang-focused example may exercise the language independently
        # of the runtime facade (for example, the graph round-trip editor).
        if "lashlang" in dependencies:
            return True
    return False


def example_manifest(source: Path) -> Path | None:
    for parent in (source.parent, *source.parents):
        if parent == EXAMPLES.parent:
            break
        manifest = parent / "Cargo.toml"
        if manifest.is_file():
            return manifest
    return None


def violations() -> list[tuple[Path, int, str]]:
    found: list[tuple[Path, int, str]] = []
    context_cache: dict[Path, bool] = {}
    for source in sorted(EXAMPLES.rglob("*.rs")):
        manifest = example_manifest(source)
        lashlang_allowed = False
        if manifest is not None:
            lashlang_allowed = context_cache.setdefault(
                manifest, is_lashlang_context(manifest)
            )
        for line_number, line in enumerate(source.read_text().splitlines(), 1):
            match = ALWAYS_FORBIDDEN.search(line)
            if match is None and not lashlang_allowed:
                match = LASHLANG.search(line)
            if match is not None:
                found.append((source.relative_to(REPO), line_number, match.group(0)))
    return found


def main() -> int:
    found = violations()
    if not found:
        print("example facade imports: no bypasses")
        return 0
    print("Examples must import host API through the lash facade:", file=sys.stderr)
    for path, line, import_path in found:
        print(f"  {path}:{line}: {import_path}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
