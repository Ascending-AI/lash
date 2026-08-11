#!/usr/bin/env python3
"""Require every PostgreSQL *_json column to have an explicit payload verdict."""

from __future__ import annotations

import json
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
SCHEMA = ROOT / "crates/lash-postgres-store/schema.sql"
SHAPE = ROOT / "crates/lash-postgres-store/schema-shape.txt"
MANIFEST = ROOT / "crates/lash-postgres-store/json-carriers.json"
TABLE = re.compile(r"^CREATE TABLE IF NOT EXISTS ([A-Za-z0-9_]+)")
JSON_COLUMN = re.compile(r"^\s+([A-Za-z0-9_]+_json)\s+")
PAYLOAD = re.compile(r"^\s+payload-shape ([A-Za-z0-9_]+\.[A-Za-z0-9_]+)\s+")
VERDICTS = {"enroll-now", "deliberately-excluded", "not-serialized-struct"}


def schema_carriers(text: str) -> set[str]:
    carriers: set[str] = set()
    table = ""
    for line in text.splitlines():
        if match := TABLE.match(line):
            table = match.group(1)
        elif match := JSON_COLUMN.match(line):
            if not table:
                raise ValueError(f"JSON column has no enclosing table: {line}")
            carriers.add(f"{table}.{match.group(1)}")
    return carriers


def enrolled_carriers(text: str) -> set[str]:
    return {match.group(1) for line in text.splitlines() if (match := PAYLOAD.match(line))}


def validate(
    carriers: set[str], enrolled: set[str], manifest: object
) -> tuple[bool, list[str]]:
    errors: list[str] = []
    if not isinstance(manifest, dict):
        return False, ["carrier manifest must be a JSON object"]
    classified = set(manifest)
    for carrier, entry in manifest.items():
        if not isinstance(entry, dict):
            errors.append(f"{carrier}: classification must be an object")
            continue
        verdict = entry.get("verdict")
        reason = entry.get("reason")
        if verdict not in VERDICTS:
            errors.append(f"{carrier}: unknown verdict {verdict!r}")
        if not isinstance(reason, str) or not reason.strip():
            errors.append(f"{carrier}: reason must be non-empty")
    for carrier in sorted(carriers - classified):
        errors.append(f"unclassified PostgreSQL JSON carrier: {carrier}")
    for carrier in sorted(classified - carriers):
        errors.append(f"classification names no schema.sql JSON carrier: {carrier}")
    declared_enrolled = {
        carrier
        for carrier, entry in manifest.items()
        if isinstance(entry, dict) and entry.get("verdict") == "enroll-now"
    }
    for carrier in sorted(declared_enrolled - enrolled):
        errors.append(f"enroll-now carrier has no payload shape: {carrier}")
    for carrier in sorted(enrolled - declared_enrolled):
        errors.append(f"payload-shaped carrier is not classified enroll-now: {carrier}")
    return not errors, errors


def main() -> int:
    try:
        carriers = schema_carriers(SCHEMA.read_text(encoding="utf-8"))
        enrolled = enrolled_carriers(SHAPE.read_text(encoding="utf-8"))
        manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
        valid, errors = validate(carriers, enrolled, manifest)
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"PostgreSQL JSON carrier coverage check could not run: {error}", file=sys.stderr)
        return 2
    if not valid:
        print("PostgreSQL JSON carrier coverage is incomplete:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print(
        f"PostgreSQL JSON carrier coverage complete: {len(carriers)} classified, "
        f"{len(enrolled)} enrolled."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
