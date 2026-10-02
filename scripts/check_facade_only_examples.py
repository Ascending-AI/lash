#!/usr/bin/env python3
"""Require example and runbook hosts to consume Lash through its facade."""

from __future__ import annotations

from pathlib import Path
import re
import sys
import tomllib
from typing import Any, Iterator

REPO = Path(__file__).resolve().parents[1]

# The workers harness mixes host code with independent storage/journal evidence.
# Ticket C1 of the runbooks sweep owns its facade cutover. Exempt exact existing
# files only; new harness sources and every other runbook host fail closed.
RUNBOOK_INTERNAL_SOURCES = frozenset(
    Path(path)
    for path in (
        'runbooks/restate-postgres-workers/src/batch_journal.rs',
        'runbooks/restate-postgres-workers/src/bin/await_event_helper.rs',
        'runbooks/restate-postgres-workers/src/bin/context_overflow_recovery.rs',
        'runbooks/restate-postgres-workers/src/bin/mock_provider/load.rs',
        'runbooks/restate-postgres-workers/src/bin/process_operations_worker.rs',
        'runbooks/restate-postgres-workers/src/bin/runner/control_scenarios.rs',
        'runbooks/restate-postgres-workers/src/bin/runner/environment.rs',
        'runbooks/restate-postgres-workers/src/bin/runner/process_assertions.rs',
        'runbooks/restate-postgres-workers/src/bin/runner/queued_work_assertions.rs',
        'runbooks/restate-postgres-workers/src/bin/runner/response_assertions.rs',
        'runbooks/restate-postgres-workers/src/bin/runner/segment_one.rs',
        'runbooks/restate-postgres-workers/src/bin/runner/tests.rs',
        'runbooks/restate-postgres-workers/src/bin/runner.rs',
        'runbooks/restate-postgres-workers/src/bin/session_operator.rs',
        'runbooks/restate-postgres-workers/src/bin/worker.rs',
        'runbooks/restate-postgres-workers/src/lib.rs',
        'runbooks/restate-postgres-workers/src/load/behavior.rs',
        'runbooks/restate-postgres-workers/src/load/behavior_tests.rs',
        'runbooks/restate-postgres-workers/src/load/behaviors.rs',
        'runbooks/restate-postgres-workers/src/load/behaviors_replay_tests.rs',
        'runbooks/restate-postgres-workers/src/load/cleanup_tests.rs',
        'runbooks/restate-postgres-workers/src/load/control.rs',
        'runbooks/restate-postgres-workers/src/load/fault_verify.rs',
        'runbooks/restate-postgres-workers/src/load/mod.rs',
        'runbooks/restate-postgres-workers/src/load/provider_watch.rs',
        'runbooks/restate-postgres-workers/src/load/tools.rs',
        'runbooks/restate-postgres-workers/src/load/upgrade_verify.rs',
        'runbooks/restate-postgres-workers/src/load/verify.rs',
        'runbooks/restate-postgres-workers/src/load/worker.rs',
        'runbooks/restate-postgres-workers/src/local_restate.rs',
        'runbooks/restate-postgres-workers/src/overflow_recovery_evidence.rs',
        'runbooks/restate-postgres-workers/src/schema_admission_tests.rs',
        'runbooks/restate-postgres-workers/src/scripted_provider.rs',
    )
)


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
                if crate in forbidden or not (crate.startswith("lash_") or crate == "lashlang"):
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
        if relative in RUNBOOK_INTERNAL_SOURCES:
            continue
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
