#!/usr/bin/env python3
"""Require a protocol bump for re-blessed worker schemas after 1.0."""
from __future__ import annotations
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]
SNAPSHOTS = "crates/lash-vm-client/tests/snapshots"
VERSION = "crates/lash-vm-protocol/src/version.rs"


def check(package_version: str, before_version: int, after_version: int,
          before: dict[str, str], after: dict[str, str]) -> list[str]:
    if int(package_version.split(".", 1)[0]) == 0 or before == after:
        return []
    errors = []
    if after_version <= before_version:
        errors.append("worker wire schema changed after 1.0 without a WORKER_PROTOCOL_VERSION bump")
    if any(after.get(name) != content for name, content in before.items()):
        errors.append("released worker schema snapshots must remain unchanged; add the new protocol snapshots")
    expected = {f"wire-v{after_version}.snap", f"wire-v{after_version}-testing.snap"}
    if not expected <= after.keys():
        errors.append("both base and testing snapshots are required for the new worker protocol version")
    return errors


def protocol(source: str) -> int:
    return int(re.search(r"pub const WORKER_PROTOCOL_VERSION: u32 = (\d+);", source)[1])


def base_ref() -> str:
    event_path = os.environ.get("GITHUB_EVENT_PATH")
    if event_path:
        event = json.loads(Path(event_path).read_text())
        if "pull_request" in event:
            return event["pull_request"]["base"]["sha"]
        if event.get("before") and set(event["before"]) != {"0"}:
            return event["before"]
    return "HEAD"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", default=base_ref())
    args = parser.parse_args()
    package_version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    if int(package_version.split(".", 1)[0]) == 0:
        print("worker wire schemas may be reviewed and refreshed in place before 1.0")
        return 0

    def git(*args):
        return subprocess.check_output(["git", *args], cwd=ROOT, text=True)

    before_source = git("show", f"{args.base}:{VERSION}")
    before = {Path(name).name: git("show", f"{args.base}:{name}") for name in
              git("ls-tree", "-r", "--name-only", args.base, "--", SNAPSHOTS).splitlines()}
    after = {path.name: path.read_text() for path in (ROOT / SNAPSHOTS).glob("*.snap")}
    errors = check(package_version, protocol(before_source), protocol((ROOT / VERSION).read_text()), before, after)
    for error in errors:
        print(error)
    return bool(errors)


if __name__ == "__main__":
    raise SystemExit(main())
