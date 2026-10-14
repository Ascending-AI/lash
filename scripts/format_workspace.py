#!/usr/bin/env python3
"""Format Lash-owned workspace members while preserving vendored source."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools/buck2"))
from clippy_policy import is_lash_owned


def main() -> int:
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked"],
        cwd=ROOT,
    ))
    members = set(metadata["workspace_members"])
    packages = sorted(
        package["name"] for package in metadata["packages"]
        if package["id"] in members
        and is_lash_owned(Path(package["manifest_path"]), ROOT)
    )
    command = ["cargo", "fmt"]
    for package in packages:
        command.extend(["--package", package])
    return subprocess.call([*command, *sys.argv[1:]], cwd=ROOT)


if __name__ == "__main__":
    raise SystemExit(main())
