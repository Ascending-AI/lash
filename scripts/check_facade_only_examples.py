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

# These files are the source-level RLM contexts in the two mixed-purpose
# examples. Agent Workbench uses them to describe and execute the RLM dialect;
# docs-snippets uses them for the RLM/Lashlang embedding pages. Keeping the
# exemption at file granularity means a new `lashlang::` import elsewhere in
# either package fails closed instead of inheriting a package-wide exemption.
RLM_LASHLANG_SOURCES = frozenset(
    Path(path)
    for path in (
        "agent-workbench/src/main_sections/plugins.rs",
        "agent-workbench/src/main_sections/routes.rs",
        "agent-workbench/src/main_sections/tests.rs",
        "agent-workbench/src/main_sections/tests/session_isolation.rs",
        "agent-workbench/src/main_sections/tests/typescript_dialect.rs",
        "agent-workbench/src/restate.rs",
        "agent-workbench/src/restate_cron_tests.rs",
        "docs-snippets/src/embedding_advanced.rs",
        "docs-snippets/src/embedding_lashlang_functions.rs",
        "docs-snippets/src/embedding_prompts.rs",
        "docs-snippets/src/embedding_typescript.rs",
        "docs-snippets/src/example_agent_workbench.rs",
        "docs-snippets/src/fig1556_preflight.rs",
    )
)

# This example is itself a Lashlang workflow-graph editor, not a host consuming
# the Lash runtime facade, so its single library target is intentionally
# Lashlang-specific. The manifest check below makes the ruling self-expire if
# that target stops depending on Lashlang.
LASHLANG_ONLY_PACKAGES = frozenset({"workflow-graph-roundtrip"})


def dependency_tables(
    document: dict[str, Any],
    sections: frozenset[str] = frozenset(
        {"dependencies", "dev-dependencies", "build-dependencies"}
    ),
) -> Iterator[dict[str, Any]]:
    for key, value in document.items():
        if key in sections:
            if isinstance(value, dict):
                yield value
        elif isinstance(value, dict):
            yield from dependency_tables(value, sections)


def dependency_features(spec: Any) -> set[str]:
    if not isinstance(spec, dict):
        return set()
    features = spec.get("features", [])
    return set(features) if isinstance(features, list) else set()


def is_lashlang_context(source: Path, manifest: Path) -> bool:
    """Whether this source is a ruled RLM or Lashlang-specific context."""
    with manifest.open("rb") as handle:
        document = tomllib.load(handle)

    try:
        source_path = source.relative_to(EXAMPLES)
    except ValueError:
        return False

    rlm_enabled = False
    lashlang_dependency = False
    for dependencies in dependency_tables(document, frozenset({"dependencies"})):
        if "rlm" in dependency_features(dependencies.get("lash")):
            rlm_enabled = True
    for dependencies in dependency_tables(document):
        if "lashlang" in dependencies:
            lashlang_dependency = True

    if source_path in RLM_LASHLANG_SOURCES:
        return rlm_enabled

    package = document.get("package", {})
    package_name = package.get("name") if isinstance(package, dict) else None
    return package_name in LASHLANG_ONLY_PACKAGES and lashlang_dependency


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
    for source in sorted(EXAMPLES.rglob("*.rs")):
        manifest = example_manifest(source)
        lashlang_allowed = manifest is not None and is_lashlang_context(
            source, manifest
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
