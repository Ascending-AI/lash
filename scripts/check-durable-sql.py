#!/usr/bin/env python3
"""Keep the durability engine's SQL in its modules (ruling #74, FIG-5163).

The engine's tables (`nodes`, `actors`, `actor_mail`; `lash_`-prefixed on
PostgreSQL) are written by one neutral statement set and one module per
dialect. This check fails when:

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

NEUTRAL = "crates/lash-store-sql/src/durable.rs"
DIALECTS = (
    "crates/lash-sqlite-store/src/durable.rs",
    "crates/lash-postgres-store/src/postgres/durable.rs",
)
ALLOWED = {
    NEUTRAL,
    *DIALECTS,
    "crates/lash-postgres-store/schema.sql",
    "crates/lash-postgres-store/teardown.sql",
}

ENGINE_SQL = re.compile(
    r"\b(?:FROM|INTO|UPDATE|JOIN|REFERENCES|ON|TABLE(?:\s+IF\s+(?:NOT\s+)?EXISTS)?)"
    r"\s+(?:main\.)?(?:lash_)?(?:nodes|actors|actor_mail)\b"
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


def stray_sql(root: Path) -> list[str]:
    findings = []
    for path in tracked(root):
        if path in ALLOWED:
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
    return " ".join(re.sub(r"\blash_(nodes|actors|actor_mail)\b", r"\1", statement).split())


def mirrored(root: Path) -> list[str]:
    seen: dict[str, str] = {}
    findings = []
    for path in DIALECTS:
        text = (root / path).read_text()
        for statement in {normalized(match.group(1)) for match in STATEMENT.finditer(text)}:
            if statement in seen and seen[statement] != path:
                findings.append(
                    f"{path}: statement also in {seen[statement]}; put it in {NEUTRAL}: {statement[:80]}"
                )
            seen.setdefault(statement, path)
    return findings


def main() -> int:
    findings = stray_sql(ROOT) + mirrored(ROOT)
    if findings:
        print("durability engine SQL outside its modules:", file=sys.stderr)
        print("\n".join(f"  {finding}" for finding in findings), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
