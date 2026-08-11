#!/usr/bin/env python3
"""Require payload artifact or fingerprint changes to bump every owning backend."""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys


DEFAULT_RANGE = "origin/main...HEAD"
ARTIFACT = "crates/lash-postgres-store/schema-shape.txt"
FINGERPRINT_ARTIFACT = "crates/lash-postgres-store/payload-schema-fingerprints.txt"
POSTGRES_VERSION_SOURCE = "crates/lash-postgres-store/src/lib.rs"
SQLITE_VERSION_SOURCE = "crates/lash-sqlite-store/src/schema.rs"
PAYLOAD_LINE = re.compile(r"^[+-]\s+(?:payload-shape|shape)\s+")
SQLITE_SHARED_PREFIX = "shape /persisted-by/sqlite/"
VERSION_LINE = re.compile(
    r"^[+-](?:pub\(crate\) )?const SCHEMA_VERSION: i32 = (\d+);"
)
# Burned one-time proof for FIG-1233's registration of the already-current
# SessionMeta shape. A registration exemption is valid only for these exact
# table/column/type and shape bytes; it is not a reusable category for future
# payloads. The fingerprint covers sorted `shape ...` lines, excluding the
# payload header itself.
REGISTRATION_BASELINES = {
    "lash_session_meta.meta_json SessionMeta": (
        "37c1eb8b238a48b82360be2ea054163c0deb9e3902f0bd56d4216964edaebdea"
    )
}
# Burned one-time proof for FIG-1283's enrollment of the already-current
# schemars documents. Only adding this complete, exact set is exempt; a later
# removal/re-addition or any changed hash still requires the owning bumps.
FINGERPRINT_REGISTRATION_BASELINES = {
    "payload-fingerprint postgres lash_session_meta.meta_json SessionMeta "
    "sha256:8a0da3fa01fac44c3785e7d3842c490c931ce91c3af1eec968890313fece9618",
    "payload-fingerprint sqlite session_meta.relation_json SessionRelation "
    "sha256:c8c90e0ab38cc10c5a900af2a561b7d2f87f9e2ef9b4b76b8f5788b7c1152da0",
}


def normalized_diff_path(path: str) -> str:
    return path[2:] if len(path) > 2 and path[1] == "/" else path


def payload_shape_change(patch: str) -> tuple[bool, bool, bool]:
    current_path = ""
    old_table = ""
    new_table = ""
    old_payload = ""
    new_payload = ""
    added_payloads: set[str] = set()
    added_shapes: dict[str, set[str]] = {}
    changed_payloads: set[str] = set()
    removed_line = False
    changed = False
    sqlite_shared_changed = False
    for line in patch.splitlines():
        if line.startswith("+++ "):
            current_path = normalized_diff_path(line[4:])
            old_table = ""
            new_table = ""
            old_payload = ""
            new_payload = ""
        elif current_path == ARTIFACT:
            content = line[1:].lstrip() if line[:1] in {"+", "-", " "} else ""
            if content.startswith("table "):
                table = content.split(maxsplit=1)[1]
                if line.startswith("-"):
                    old_table = table
                elif line.startswith("+"):
                    new_table = table
                else:
                    old_table = table
                    new_table = table
                old_payload = ""
                new_payload = ""
            elif content.startswith("payload-shape "):
                _, column, rust_type = content.split(maxsplit=2)
                if line.startswith("+"):
                    new_payload = payload_identity(new_table, column, rust_type)
                    added_payloads.add(new_payload)
                    added_shapes.setdefault(new_payload, set())
                    changed = True
                elif line.startswith("-"):
                    old_payload = payload_identity(old_table, column, rust_type)
                    removed_line = True
                    changed = True
                else:
                    old_payload = payload_identity(old_table, column, rust_type)
                    new_payload = payload_identity(new_table, column, rust_type)
            elif content.startswith("shape ") and PAYLOAD_LINE.match(line):
                changed = True
                if content.startswith(SQLITE_SHARED_PREFIX):
                    sqlite_shared_changed = True
                if line.startswith("+"):
                    changed_payloads.add(new_payload)
                    if new_payload in added_shapes:
                        added_shapes[new_payload].add(content)
                else:
                    changed_payloads.add(old_payload)
                    removed_line = True
    registration_only = (
        changed
        and not removed_line
        and bool(added_payloads)
        and changed_payloads <= added_payloads
        and all(
            REGISTRATION_BASELINES.get(payload)
            == shape_fingerprint(added_shapes.get(payload, set()))
            for payload in added_payloads
        )
    )
    return changed, registration_only, sqlite_shared_changed


def payload_fingerprint_change(patch: str) -> tuple[set[str], bool]:
    current_path = ""
    changed: set[str] = set()
    added: set[str] = set()
    removed = False
    for line in patch.splitlines():
        if line.startswith("+++ "):
            current_path = normalized_diff_path(line[4:])
            continue
        if current_path != FINGERPRINT_ARTIFACT or line[:1] not in {"+", "-"}:
            continue
        content = line[1:].strip()
        if content.startswith("payload-fingerprint "):
            _, backend, *_ = content.split()
            changed.add(backend)
            if line.startswith("+"):
                added.add(content)
            else:
                removed = True
    registration_only = (
        bool(changed)
        and not removed
        and added == FINGERPRINT_REGISTRATION_BASELINES
    )
    return changed, registration_only


def payload_identity(table: str, column: str, rust_type: str) -> str:
    qualified_column = column if "." in column else f"{table}.{column}"
    return f"{qualified_column} {rust_type}"


def shape_fingerprint(lines: set[str]) -> str:
    canonical = "\n".join(sorted(lines)).encode("utf-8")
    return hashlib.sha256(canonical).hexdigest()


def component_version_advanced(patch: str, version_source: str) -> bool:
    current_path = ""
    removed: set[int] = set()
    added: set[int] = set()
    for line in patch.splitlines():
        if line.startswith("+++ "):
            current_path = normalized_diff_path(line[4:])
            continue
        if current_path != version_source:
            continue
        match = VERSION_LINE.match(line)
        if match is None:
            continue
        target = added if line.startswith("+") else removed
        target.add(int(match.group(1)))
    return len(removed) == 1 and len(added) == 1 and next(iter(added)) > next(iter(removed))


def validate_patch(patch: str) -> tuple[bool, str]:
    changed, registration_only, sqlite_shared_changed = payload_shape_change(patch)
    fingerprint_backends, fingerprint_registration_only = payload_fingerprint_change(patch)
    required_backends = fingerprint_backends.copy()
    if changed:
        required_backends.add("postgres")
    if sqlite_shared_changed:
        required_backends.add("sqlite")
    if not required_backends:
        return True, "No durable payload shapes or fingerprints changed."
    if registration_only and not fingerprint_backends:
        return True, "A previously unobserved PostgreSQL payload shape was registered."
    if fingerprint_registration_only and not changed:
        return True, "The already-current payload schemas received their initial fingerprints."
    postgres_advanced = component_version_advanced(patch, POSTGRES_VERSION_SOURCE)
    sqlite_advanced = component_version_advanced(patch, SQLITE_VERSION_SOURCE)
    postgres_required = "postgres" in required_backends
    sqlite_required = "sqlite" in required_backends
    if (not postgres_required or postgres_advanced) and (not sqlite_required or sqlite_advanced):
        if postgres_required and sqlite_required:
            return True, "Shared payload contract change advances both durable backend versions."
        backend = "PostgreSQL" if postgres_required else "SQLite"
        return True, f"{backend} payload contract change advances its component version."
    missing = []
    if postgres_required and not postgres_advanced:
        missing.append(f"PostgreSQL SCHEMA_VERSION in {POSTGRES_VERSION_SOURCE}")
    if sqlite_required and not sqlite_advanced:
        missing.append(f"SQLite SCHEMA_VERSION in {SQLITE_VERSION_SOURCE}")
    return (
        False,
        "Durable payload shape or fingerprint changed without advancing "
        + " and ".join(missing)
        + ".",
    )


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--cached", action="store_true", help="inspect the staged diff")
    source.add_argument("--range", dest="revision_range", help="inspect REV_RANGE")
    source.add_argument("--diff-file", type=Path, help="inspect a saved unified diff")
    return parser.parse_args(argv)


def default_range() -> str:
    base = os.environ.get("GITHUB_BASE_REF")
    return f"origin/{base}...HEAD" if base else DEFAULT_RANGE


def load_patch(args: argparse.Namespace) -> str:
    if args.diff_file is not None:
        return args.diff_file.read_text(encoding="utf-8")
    command = ["git", "diff", "--no-color", "--no-ext-diff"]
    if args.cached:
        command.append("--cached")
    else:
        command.append(args.revision_range or default_range())
    return subprocess.run(command, check=True, capture_output=True, text=True).stdout


def main(argv: list[str]) -> int:
    try:
        valid, message = validate_patch(load_patch(parse_args(argv)))
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"PostgreSQL payload-shape version check could not run: {error}", file=sys.stderr)
        return 2
    print(message, file=sys.stdout if valid else sys.stderr)
    return 0 if valid else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
