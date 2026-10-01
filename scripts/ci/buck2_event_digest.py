#!/usr/bin/env python3
"""Append a compact Buck2 event-log summary and critical path to CI output."""

from __future__ import annotations

import argparse
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[2]


def client() -> str:
    result = subprocess.run(
        [sys.executable, "tools/buck2/bootstrap.py"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    executable = result.stdout.strip()
    if not executable:
        raise RuntimeError("Buck2 bootstrap returned no executable")
    return executable


def command(executable: str, *args: str) -> str:
    result = subprocess.run(
        [executable, "log", *args],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout.strip()


def digest(event_log: Path) -> list[str]:
    executable = client()
    summary = command(executable, "summary", str(event_log))
    critical = command(
        executable,
        "critical-path",
        "--format",
        "readable",
        str(event_log),
    )
    critical_lines = critical.splitlines()
    return [
        "Buck2 summary:",
        *(f"  {line}" for line in summary.splitlines()),
        "Critical path:",
        *(f"  {line}" for line in critical_lines[:7]),
    ]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("event_log", type=Path)
    parser.add_argument("--summary", type=Path, help="append to GitHub step summary")
    args = parser.parse_args()
    if not args.event_log.is_file():
        print(f"Buck2 event log unavailable: {args.event_log}")
        return 0
    try:
        lines = digest(args.event_log)
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"::warning::Could not summarize Buck2 event log: {error}", file=sys.stderr)
        return 0
    print("\n".join(lines))
    if args.summary:
        with args.summary.open("a", encoding="utf-8") as stream:
            stream.write("### Buck2 build event log\n\n")
            stream.write("\n".join(f"- {line}" for line in lines))
            stream.write("\n\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
