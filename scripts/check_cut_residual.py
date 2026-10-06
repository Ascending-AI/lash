#!/usr/bin/env python3
"""Require a regenerated cut candidate to contain only reset outputs and gate activation."""

import argparse
import io
import json
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile

import release_reset


def git(repo: Path, *args: str) -> str:
    return subprocess.check_output(["git", "-C", str(repo), *args], text=True)


def reset_paths(plan: dict) -> set[str]:
    paths = {"Cargo.toml"}
    for key in ("source_edits", "generated_paths", "fixture_paths", "build_inventory_paths"):
        paths.update(plan[key])
    paths.update(plan["schema_renames"])
    paths.update(plan["schema_renames"].values())
    return paths


def activation_only(before: str, after: str) -> bool:
    """Only required-check flags or dependencies may differ in a workflow."""
    def normalized(text: str) -> list[str]:
        lines = []
        job = None
        for line in text.splitlines():
            match = re.fullmatch(r"  ([A-Za-z0-9_-]+):\s*", line)
            if match:
                job = match[1]
            if (job == "version-bumps"
                    and re.fullmatch(r"    continue-on-error: (true|false)\s*", line)):
                continue
            if re.fullmatch(r"\s*- version-bumps\s*", line):
                continue
            if "needs:" in line:
                line = re.sub(r"\bversion-bumps,?\s*", "", line)
                line = re.sub(r",\s*\]", "]", line)
            lines.append(line)
        return lines

    # Making an advisory check required cannot add a new exemption.
    if after.count("continue-on-error: true") > before.count("continue-on-error: true"):
        return False
    if after.count("version-bumps") < before.count("version-bumps"):
        return False
    return normalized(before) == normalized(after)


def check(repo: Path, base: str, head: str, plan: dict) -> dict:
    subprocess.run(["git", "-C", str(repo), "merge-base", "--is-ancestor", base, head], check=True)
    paths = git(repo, "diff", "--no-renames", "--name-only", base, head).splitlines()
    allowed = reset_paths(plan)
    unexpected = []
    for path in paths:
        if path in allowed:
            continue
        if path.startswith(".github/workflows/") and path.endswith(".yml"):
            try:
                before = git(repo, "show", f"{base}:{path}")
                after = git(repo, "show", f"{head}:{path}")
            except subprocess.CalledProcessError:
                unexpected.append(path)
                continue
            if activation_only(before, after):
                continue
        unexpected.append(path)
    return {"base": base, "head": head, "changed_paths": len(paths), "unexpected_paths": unexpected}


def base_plan(repo: Path, base: str) -> dict:
    scratch = repo / ".buck2/cut-residual"
    scratch.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=scratch) as directory:
        archive = subprocess.check_output(["git", "-C", str(repo), "archive", base])
        with tarfile.open(fileobj=io.BytesIO(archive)) as tree:
            tree.extractall(directory, filter="data")
        plan, _ = release_reset.plan(Path(directory))
        return plan


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=release_reset.baseline.ROOT)
    parser.add_argument("--base", default="origin/main")
    parser.add_argument("--head", default="HEAD")
    args = parser.parse_args()
    try:
        plan = base_plan(args.repo, args.base)
        result = check(args.repo, args.base, args.head, plan)
        print(json.dumps(result, indent=2))
        return int(bool(result["unexpected_paths"]))
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print(f"cut residual check failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
