#!/usr/bin/env python3
"""Hold the test-case floors the retired Cargo feature legs carried.

`Runtime feature boundary (default-off-tests)` did not only run the default
build's test suite: it counted the cases and failed when the count fell, so a
test accidentally moved behind a feature could not take the default build's
coverage with it silently. The suite itself is now a pool test target; this
restores the count.

The labels and floors come from `tools/buck2/target-inventory.json`, which the
generator writes. A variant's name is a hash of its resolved closure, so no
caller reconstructs it. A lane command's name filter (`cargo test ... --lib
conformance`) is part of what the variant executes, so the count applies the
same libtest arguments: a floor on a filtered selection counts that selection.
"""

from __future__ import annotations

import json
import os
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
INVENTORY = ROOT / "tools" / "buck2" / "target-inventory.json"
CASE = re.compile(r": test$", re.MULTILINE)


def floors() -> tuple[dict[str, int], dict[str, list[str]]]:
    inventory = json.loads(INVENTORY.read_text(encoding="utf-8"))
    values = inventory["feature_lane_test_floors"]
    if not isinstance(values, dict) or not all(
        isinstance(label, str) and isinstance(floor, int)
        for label, floor in values.items()
    ):
        raise ValueError("feature_lane_test_floors must map labels to integers")
    arguments = inventory["feature_lane_test_args"]
    if not isinstance(arguments, dict) or not all(
        isinstance(label, str)
        and isinstance(args, list)
        and all(isinstance(arg, str) for arg in args)
        for label, args in arguments.items()
    ):
        raise ValueError("feature_lane_test_args must map labels to string lists")
    return values, arguments


def main() -> int:
    expected, arguments = floors()
    if not expected:
        raise SystemExit("feature_lane_test_floors is empty")
    report_dir = pathlib.Path(os.environ.get("RUNNER_TEMP", ROOT / ".buck2" / "ci-reports"))
    report_dir.mkdir(parents=True, exist_ok=True)
    report = report_dir / "feature-lane-floor-build-report.json"
    subprocess.run(
        [
            "scripts/hermetic-build.sh",
            "build",
            "--jobs",
            "32" if os.environ.get("CI") else "16",
            "--materializations",
            "final",
            "--build-report",
            str(report),
            *sorted(expected),
        ],
        cwd=ROOT,
        check=True,
    )
    failures = []
    for label, floor in sorted(expected.items()):
        resolved = subprocess.run(
            [
                sys.executable,
                "tools/buck2/outputs.py",
                "--report",
                str(report),
                "--label",
                label,
                "--single",
            ],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        )
        executable = resolved.stdout.strip()
        listing = subprocess.run(
            [executable, *arguments.get(label, []), "--list"],
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
