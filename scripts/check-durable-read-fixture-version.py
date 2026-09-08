#!/usr/bin/env python3
"""Require durable-read artifact changes to declare a fixture schema bump."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import re
import subprocess
import sys


DEFAULT_RANGE = "origin/main...HEAD"
FIXTURE_PREFIX = "fixtures/durable-read/"
VERSION_SOURCE = "crates/lash-core/tests/support/durable_read_fixture.rs"
VERSION_LINE = re.compile(
    r"^[+-]\s*pub const DURABLE_READ_FIXTURE_SCHEMA_VERSION:\s*u32\s*=\s*\d+;"
)


def normalized_diff_path(path: str) -> str:
    return path[2:] if len(path) > 2 and path[1] == "/" else path


def changed_paths(patch: str) -> set[str]:
    paths: set[str] = set()
    for line in patch.splitlines():
        if line.startswith("diff --git "):
            before_and_after = line.split(maxsplit=3)[2:]
            paths.update(normalized_diff_path(path) for path in before_and_after)
            continue
        if not line.startswith("+++ "):
            continue
        path = line[4:]
        if path == "/dev/null":
            continue
        paths.add(normalized_diff_path(path))
    return paths


def artifact_paths(patch: str) -> list[str]:
    return sorted(
        path
        for path in changed_paths(patch)
        if path.startswith(FIXTURE_PREFIX) and not path.endswith("/README.md")
    )


def declares_fixture_version_change(patch: str) -> bool:
    current_path = ""
    for line in patch.splitlines():
        if line.startswith("+++ "):
            path = line[4:]
            current_path = normalized_diff_path(path)
        elif current_path == VERSION_SOURCE and VERSION_LINE.match(line):
            return True
    return False


def validate_patch(patch: str) -> tuple[bool, str]:
    artifacts = artifact_paths(patch)
    if not artifacts:
        return True, "No durable-read fixture artifacts changed."
    if declares_fixture_version_change(patch):
        return (
            True,
            "Durable-read fixture artifact change includes a fixture schema-version change.",
        )
    rendered = "\n  - ".join(artifacts)
    return (
        False,
        "Durable-read fixture artifacts changed without changing "
        "DURABLE_READ_FIXTURE_SCHEMA_VERSION in "
        f"{VERSION_SOURCE}:\n  - {rendered}",
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


def decode_patch(raw: bytes) -> str:
    # Diffs may embed non-UTF-8 payload bytes (e.g. committed fuzz corpus
    # seeds). Decode leniently: the lines this gate inspects ("diff --git ",
    # "+++ ", and the DURABLE_READ_FIXTURE_SCHEMA_VERSION constant) are always
    # valid UTF-8, so replacement characters in payload lines cannot mask a
    # durable-read fixture change or a missing schema-version bump.
    return raw.decode("utf-8", errors="replace")


def load_patch(args: argparse.Namespace) -> str:
    if args.diff_file is not None:
        return decode_patch(args.diff_file.read_bytes())
    command = ["git", "diff", "--no-color", "--no-ext-diff"]
    if args.cached:
        command.append("--cached")
    else:
        command.append(args.revision_range or default_range())
    return decode_patch(subprocess.run(command, check=True, capture_output=True).stdout)


def main(argv: list[str]) -> int:
    try:
        valid, message = validate_patch(load_patch(parse_args(argv)))
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"durable-read fixture version check could not run: {error}", file=sys.stderr)
        return 2
    print(message, file=sys.stderr if not valid else sys.stdout)
    return 0 if valid else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
