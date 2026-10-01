#!/usr/bin/env python3
"""Summarize failed Buck2 test targets and stage their declared outputs."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import shutil
import sys
import xml.etree.ElementTree as ET


CASE_LIMIT = 50
PASS_STATUSES = {"PASS", "SUCCESS"}


def failing_cases(report: Path) -> list[str]:
    try:
        root = ET.parse(report).getroot()
    except (OSError, ET.ParseError):
        return []
    return [
        f"{case.get('classname', '')}::{case.get('name', '')}"
        for case in root.iter("testcase")
        if case.find("failure") is not None or case.find("error") is not None
    ]


def failed_targets(report: dict) -> list[tuple[str, dict]]:
    return [
        (label.removeprefix("root"), result)
        for label, result in sorted(report.get("results", {}).items())
        if result.get("status") not in PASS_STATUSES
    ]


def stage_result(label: str, result: dict, staging: Path) -> Path:
    package, _, target = label.removeprefix("//").partition(":")
    destination = staging / package / target
    destination.mkdir(parents=True, exist_ok=True)
    for key, source_text in (result.get("outputs") or {}).items():
        if not source_text:
            continue
        source = Path(source_text)
        if not source.exists():
            continue
        name = {"junit_xml": "test.xml", "log": "test.log"}.get(key, key)
        if source.is_dir():
            shutil.copytree(source, destination / name, dirs_exist_ok=True)
        else:
            shutil.copy2(source, destination / name)
    return destination


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--stage", type=Path, required=True)
    parser.add_argument("--summary", type=Path)
    args = parser.parse_args()
    try:
        report = json.loads(args.report.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        print(f"::warning::Buck2 test report unavailable: {error}", file=sys.stderr)
        return 0

    lines = ["### Failed test targets", ""]
    targets = failed_targets(report)
    if not targets:
        lines.append("No failed test targets found in the test report.")
    for label, result in targets:
        stage_result(label, result, args.stage)
        outputs = result.get("outputs") or {}
        cases = failing_cases(Path(outputs["junit_xml"])) if outputs.get("junit_xml") else []
        notes = [f"exit {result.get('exit_code')}" if result.get("exit_code") is not None else "no exit code"]
        if result.get("cache"):
            notes.append(str(result["cache"]))
        if cases:
            notes.append(f"{len(cases)} failing case(s)")
        lines.append(f"- `{label}` {result.get('status', 'UNKNOWN')} ({'; '.join(notes)})")
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
