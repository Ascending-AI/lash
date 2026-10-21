#!/usr/bin/env python3
"""Keep the durability engine's SQL in its modules (ruling #74, FIG-5163).

The engine's tables (`nodes`, `actors`, `actor_mail`, and the domain tables
the runtime lanes add to the fenced commit: `run_records`, `exec_snapshots`,
`waits`, `turn_phases`, `turn_namespaces`, `session_close`, `session_scope_ends`,
`prompt_snapshots`, `prompt_texts`, `prompt_snapshot_texts`, and the checkpoint graph's
`effect_admissions`, the persistent heap's `root_leases` and
`retirement_holds`, and the execution package rows `package_executions` and `execution_resolutions`;
`lash_`-prefixed on PostgreSQL) are written by one
neutral statement set and one module per dialect. Since I0 (FIG-5194) each
of those is a directory. Any file in the durable module directory belongs
to that module; membership comes from tracked files, not a domain-name list.
PostgreSQL's one-call owner commit (FIG-5936, FIG-5942) also keeps its step
programs, executor and server-function generator in that directory. This check fails
when:

- SQL naming an engine table appears in any tracked Rust or SQL file outside
  those modules and the PostgreSQL schema artifacts; a second writer or reader
  would bypass the epoch fence the modules apply;
- a dialect module carries a statement the other dialect module also carries,
  once table prefixes and whitespace are set aside: a statement both dialects
  issue verbatim belongs in the neutral set, never mirrored by hand.

SQL is matched by its keywords in upper case (`FROM actors`,
`INSERT INTO lash_nodes`, `REFERENCES lash_actors`), so prose that mentions
actors is not SQL.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

NEUTRAL_DIR = "crates/lash-store-sql/src/durable"
DIALECT_DIRS = (
    "crates/lash-sqlite-store/src/durable",
    "crates/lash-postgres-store/src/postgres/durable",
)
SCHEMA_ARTIFACTS = {
    "crates/lash-postgres-store/schema.sql",
    "crates/lash-postgres-store/teardown.sql",
}

TABLES = (
    "nodes",
    "actors",
    "actor_mail",
    "run_records",
    "exec_snapshots",
    "turn_phases",
    "turn_namespaces",
    "waits",
    "session_close",
    "session_scope_ends",
    "prompt_snapshots",
    "prompt_texts",
    "prompt_snapshot_texts",
    "effect_admissions",
    "package_labels",
    "package_executions",
    "execution_resolutions",
    "owner_commit_latches",
    "root_leases",
    "retirement_holds",
)

ENGINE_SQL = re.compile(
    r"\b(?:FROM|INTO|UPDATE|JOIN|REFERENCES|ON|TABLE(?:\s+IF\s+(?:NOT\s+)?EXISTS)?)"
    r"\s+(?:main\.)?(?:lash_)?(?:" + "|".join(TABLES) + r")\b"
)

STATEMENT = re.compile(r'^\s*\w+\s*=\s*"((?:[^"\\]|\\.)*)";', re.MULTILINE | re.DOTALL)


def tracked(root: Path) -> list[str]:
    listed = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs", "*.sql"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return [path for path in listed.split("\0") if path]


def module_files(paths: list[str]) -> set[str]:
    """Every tracked Rust file directly in the durable module directories."""
    directories = {NEUTRAL_DIR, *DIALECT_DIRS}
    return {
        path
        for path in paths
        if Path(path).suffix == ".rs" and Path(path).parent.as_posix() in directories
    }


def stray_sql(root: Path, paths: list[str], allowed: set[str]) -> list[str]:
    findings = []
    for path in paths:
        if path in allowed:
            continue
        try:
            text = (root / path).read_text()
        except (OSError, UnicodeDecodeError):
            continue
        for match in ENGINE_SQL.finditer(text):
            line = text.count("\n", 0, match.start()) + 1
            findings.append(f"{path}:{line}: engine-table SQL outside its modules: {match.group(0)!r}")
    return findings


def normalized(statement: str) -> str:
    return " ".join(re.sub(r"\blash_(" + "|".join(TABLES) + r")\b", r"\1", statement).split())


def mirrored(root: Path, modules: set[str]) -> list[str]:
    seen: dict[str, str] = {}
    findings = []
    for path in sorted(modules):
        dialect = Path(path).parent.as_posix()
        if dialect not in DIALECT_DIRS:
            continue
        try:
            text = (root / path).read_text()
        except OSError:
            continue
        for statement in {normalized(match.group(1)) for match in STATEMENT.finditer(text)}:
            if statement in seen and not seen[statement].startswith(dialect):
                findings.append(
                    f"{path}: statement also in {seen[statement]}; put it in {NEUTRAL_DIR}: {statement[:80]}"
                )
            seen.setdefault(statement, path)
    return findings


def main() -> int:
    paths = tracked(ROOT)
    modules = module_files(paths)
    allowed = modules | SCHEMA_ARTIFACTS
    findings = stray_sql(ROOT, paths, allowed) + mirrored(ROOT, modules)
    if findings:
        print("durability engine SQL outside its modules:", file=sys.stderr)
        print("\n".join(f"  {finding}" for finding in findings), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
