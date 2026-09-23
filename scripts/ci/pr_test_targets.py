#!/usr/bin/env python3
"""Select deterministic PR tests; use the full suite when selection is uncertain."""

from __future__ import annotations

import argparse
import ast
import base64
import os
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[2]
FULL_SUITE = ["//:workspace_tests"]


def inventory() -> tuple[set[str], dict[str, list[str]]]:
    tree = ast.parse((ROOT / "tools/bazel/workspace_targets.bzl").read_text())
    names = {"WORKSPACE_BAZEL_TEST_TARGETS", "WORKSPACE_TEST_BATCHES"}
    values = {
        node.targets[0].id: ast.literal_eval(node.value)
        for node in tree.body
        if isinstance(node, ast.Assign)
        and len(node.targets) == 1
        and isinstance(node.targets[0], ast.Name)
        and node.targets[0].id in names
    }
    return set(values["WORKSPACE_BAZEL_TEST_TARGETS"]), values["WORKSPACE_TEST_BATCHES"]


def packages_for(paths: list[str]) -> list[str] | None:
    packages: set[str] = set()
    for name in paths:
        path = Path(name)
        # Package README files are inputs to some integration tests. In a
        # mixed source/docs PR, ignoring them could omit affected tests.
        if path.suffix == ".md":
            return None
        if name.startswith("docs/") or name in {"LICENSE", ".gitignore"}:
            continue
        if (len(path.parts) < 3 or path.parts[0] not in {"crates", "examples", "runbooks"}
                or path.suffix != ".rs"):
            return None
        package = "/".join(path.parts[:2])
        if not (ROOT / package / "BUILD.bazel").is_file():
            return None
        packages.add("//" + package)
    return sorted(packages) if packages else None


def compact(members: set[str], batches: dict[str, list[str]]) -> list[str]:
    labels = set(members)
    for batch, children in batches.items():
        if children and set(children) <= labels:
            labels.difference_update(children)
            labels.add(batch)
    return sorted(labels)


def select(paths: list[str], query_output: str, allowed: set[str],
           batches: dict[str, list[str]]) -> list[str]:
    packages = packages_for(paths)
    if packages is None:
        return FULL_SUITE
    found = {line for line in query_output.splitlines() if line.startswith("//")}
    if len(found) != len(query_output.splitlines()):
        return FULL_SUITE
    members = found & allowed
    # A source package with no deterministic test is still compiled by the
    # workspace Clippy job, but a surprising empty graph is not a reason to
    # silently drop test proof here.
    if not members:
        return FULL_SUITE
    return compact(members, batches)


def git(*args: str) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(["git", *args], cwd=ROOT, capture_output=True, check=False)


def changed_paths(base: str) -> list[str] | None:
    if git("cat-file", "-e", base + "^{commit}").returncode:
        token = os.environ.get("GITHUB_TOKEN")
        if not token:
            return None
        header = base64.b64encode(f"x-access-token:{token}".encode()).decode()
        fetched = git("-c", f"http.extraheader=AUTHORIZATION: basic {header}",
                      "fetch", "--no-tags", "--depth=1", "origin", base)
        if fetched.returncode:
            return None
    diff = git("diff", "--name-status", "--no-renames", "-z", base, "HEAD")
    if diff.returncode or git("merge-base", "--is-ancestor", base, "HEAD").returncode:
        return None
    fields = diff.stdout.split(b"\0")
    if fields[-1:] == [b""]:
        fields.pop()
    if len(fields) % 2:
        return None
    if any(fields[index] not in {b"A", b"M"} for index in range(0, len(fields), 2)):
        return None
    return [os.fsdecode(fields[index]) for index in range(1, len(fields), 2)]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True)
    parser.add_argument("--output-user-root", required=True)
    args = parser.parse_args()
    paths = changed_paths(args.base)
    reason = "unclassified diff"
    labels = FULL_SUITE
    if paths is not None:
        packages = packages_for(paths)
        if packages is not None:
            try:
                allowed, batches = inventory()
                expression = 'kind("test", rdeps(set(//crates/... //examples/... //runbooks/...), set(' + " ".join(
                    package + ":all" for package in packages) + ')))'
                query = subprocess.run(
                    ["bazel", f"--output_user_root={args.output_user_root}",
                     "query", expression], cwd=ROOT, capture_output=True,
                    text=True, timeout=60, check=False,
                )
                if query.returncode == 0:
                    labels = select(paths, query.stdout, allowed, batches)
                    reason = "reverse dependencies" if labels != FULL_SUITE else "empty or invalid graph"
                else:
                    reason = "graph query failed"
            except (OSError, KeyError, SyntaxError, ValueError, subprocess.TimeoutExpired):
                reason = "inventory or graph unavailable"
    print(f"PR Bazel tests: {reason}; {len(labels)} label(s)", file=sys.stderr)
    print("\n".join(labels))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
