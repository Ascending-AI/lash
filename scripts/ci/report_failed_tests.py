#!/usr/bin/env python3
"""Name the tests a Bazel leg failed and stage their logs for upload.

A pool ``bazel test`` leg prints a one-line verdict per target, and the full
evidence -- ``test.log``, the JUnit ``test.xml`` and the undeclared outputs --
lands under ``bazel-testlogs`` only where the download regex materialized it.
The failing leg tees its console output to a file; this reads that file, names
each failed target and, when the target's JUnit report survived, its failing
cases, then copies each target's whole testlogs directory into a staging
directory the job's ``upload-artifact`` step publishes. A target killed by a
signal is reported as such: the executor killing the action is a diagnosis no
test edit can supply.

The report is a reading of a failure, never a verdict of its own: any problem
producing it warns and exits zero.
"""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import shutil
import sys
import xml.etree.ElementTree as ET


ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
# `//pkg:target FAILED in 62.7s`, optionally after a timestamp and carrying
# parentheticals (`(shard 2 of 4)`, `(cached)`, `(run 1 of 30)`).
VERDICT = re.compile(
    r"(//[^\s:]+:[^\s]+?)\s+(?:\([^)]*\)\s*)*(FAILED|TIMEOUT|FLAKY)\b"
)
# `FAIL: //pkg:target (Killed) (see /path/test.log)` -- the banner Bazel prints
# the moment the action returns; the parenthetical carries the kill signal.
BANNER = re.compile(
    r"^\s*(?:\([^)]*\)\s*)?FAIL:\s*(//[^\s:]+:[^\s(]+)\s*(?:\(([^)]*)\))?"
)
CASE_LIMIT = 50


def failed_targets(output: str) -> list[dict[str, object]]:
    """Ordered unique `{"label", "verdict", "killed"}` rows from the stream."""
    rows: dict[str, dict[str, object]] = {}
    for raw in output.splitlines():
        line = ANSI.sub("", raw)
        banner = BANNER.search(line)
        if banner is not None:
            label = banner.group(1)
            row = rows.setdefault(
                label, {"label": label, "verdict": "FAILED", "killed": False}
            )
            if (banner.group(2) or "").lower() == "killed":
                row["killed"] = True
            continue
        match = VERDICT.search(line)
        if match is not None:
            label, verdict = match.groups()
            row = rows.setdefault(
                label, {"label": label, "verdict": verdict, "killed": False}
            )
            if row["verdict"] not in {"FAILED", "TIMEOUT"}:
                row["verdict"] = verdict
    return list(rows.values())


def testlogs_dir(testlogs: Path, label: str) -> Path:
    """`//a/b:c` -> `<testlogs>/a/b/c` (canonical `@@//` labels collapse)."""
    target = label.split("//", 1)[1]
    package, _, name = target.partition(":")
    return testlogs / package / name


def failing_cases(directory: Path) -> list[str]:
    """Failed/error test case names across every JUnit report in the dir."""
    names: list[str] = []
    for report in sorted(directory.rglob("test.xml")):
        try:
            root = ET.parse(report).getroot()
        except ET.ParseError:
            continue
        for suite in [root, *root.findall("testsuite")]:
            for case in suite.iter("testcase"):
                if case.find("failure") is not None or case.find("error") is not None:
                    names.append(
                        f"{case.get('classname', '')}::{case.get('name', '')}"
                    )
    return names


def stage(source: Path, testlogs: Path, staging: Path) -> Path | None:
    """Copy one target's testlogs tree into the staging root, mirroring its
    relative path (`<stage>/crates/lash/lash__unit_test/...`)."""
    if not source.is_dir():
        return None
    destination = staging / source.relative_to(testlogs)
    destination.mkdir(parents=True, exist_ok=True)
    for item in sorted(source.iterdir()):
        if item.is_dir():
            shutil.copytree(item, destination / item.name, dirs_exist_ok=True)
        else:
            shutil.copy2(item, destination / item.name)
    return destination


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output",
        type=Path,
        required=True,
        help="the bazel leg's tee'd console output",
    )
    parser.add_argument(
        "--testlogs", type=Path, required=True, help="the bazel-testlogs directory"
    )
    parser.add_argument(
        "--stage",
        type=Path,
        required=True,
        help="directory the artifact upload publishes",
    )
    parser.add_argument(
        "--summary",
        type=Path,
        help="append a markdown section (GITHUB_STEP_SUMMARY)",
    )
    args = parser.parse_args()

    try:
        output = args.output.read_text(encoding="utf-8", errors="replace")
    except OSError:
        print(f"Bazel console output unavailable: {args.output}")
        return 0

    lines = ["### Failed test targets", ""]
    targets = failed_targets(output)
    if not targets:
        lines.append("No failed test targets found in the console output.")
        print(lines[-1])
    for row in targets:
        directory = testlogs_dir(args.testlogs, str(row["label"]))
        notes: list[str] = []
        cases: list[str] = []
        if row["killed"]:
            notes.append("killed by a signal (executor kill)")
        if not directory.is_dir():
            notes.append("no testlogs directory")
        else:
            cases = failing_cases(directory)
            stage(directory, args.testlogs, args.stage)
            if not list(directory.rglob("test.log")):
                notes.append(
                    "no test.log -- the action left no output (executor/OOM kill)"
                )
            if cases:
                notes.append(f"{len(cases)} failing case(s)")
        lines.append(
            f"- `{row['label']}` {row['verdict']}"
            + (f" — {'; '.join(notes)}" if notes else "")
        )
        for name in cases[:CASE_LIMIT]:
            lines.append(f"  - `{name}`")
        if len(cases) > CASE_LIMIT:
            lines.append(f"  - ... and {len(cases) - CASE_LIMIT} more")

    print("\n".join(lines[2:]))
    if args.summary:
        try:
            with args.summary.open("a", encoding="utf-8") as stream:
                stream.write("\n".join(lines) + "\n\n")
        except OSError as error:
            print(f"::warning::step summary not written: {error}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
