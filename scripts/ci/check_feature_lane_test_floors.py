#!/usr/bin/env python3
"""Hold the test-case floors the retired Cargo feature legs carried.

`Runtime feature boundary (default-off-tests)` did not only run the default
build's test suite: it counted the cases and failed when the count fell, so a
test accidentally moved behind a feature could not take the default build's
coverage with it silently. The suite itself is now a pool test target; this
restores the count.

The labels come from `tools/bazel/feature_lanes.bzl`, which the generator
writes -- the variant's name is a hash of its resolved closure, so nothing else
can name it.
"""

from __future__ import annotations

import ast
import os
import pathlib
import re
import shlex
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
LANES = ROOT / "tools" / "bazel" / "feature_lanes.bzl"
CASE = re.compile(r": test$", re.MULTILINE)


def floors() -> dict[str, int]:
    source = LANES.read_text(encoding="utf-8")
    marker = "FEATURE_LANE_TEST_FLOORS = "
    start = source.index(marker) + len(marker)
    end = source.index("\n\n", start)
    return ast.literal_eval(source[start:end])


def main() -> int:
    flags = shlex.split(os.environ.get("BAZEL_SHARED_CACHE_FLAGS", ""))
    failures = []
    for label, floor in sorted(floors().items()):
        listing = subprocess.run(
            ["bazel", "run", *flags, label, "--", "--list"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        count = len(CASE.findall(listing))
        print(f"{label}: {count} cases (floor {floor})")
        if count < floor:
            failures.append(f"{label} regressed to {count} cases, floor is {floor}")
    for failure in failures:
        print(failure, file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
