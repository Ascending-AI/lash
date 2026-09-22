#!/usr/bin/env python3
"""Print a short, wall-clock-safe digest of a Bazel JSON trace profile."""

from __future__ import annotations

import argparse
import gzip
import json
import math
from pathlib import Path
import sys


def digest(profile: dict) -> list[str]:
    events = profile["traceEvents"]
    if not isinstance(events, list):
        raise ValueError("traceEvents is not a list")
    critical = sorted(
        (event for event in events if event.get("cat") == "critical path component"
         and isinstance(event.get("dur"), (int, float))),
        key=lambda event: event["dur"],
        reverse=True,
    )
    queues = sorted(
        event["dur"] / 1_000_000
        for event in events
        if event.get("cat") == "Remote execution queuing time"
        and isinstance(event.get("dur"), (int, float))
    )
    lines = [
        f"Critical path: {sum(event['dur'] for event in critical) / 1_000_000:.2f}s "
        f"across {len(critical)} component{'s' if len(critical) != 1 else ''} "
        "(sequential path, not all action time)."
    ]
    for event in critical[:5]:
        name = " ".join(str(event.get("name", "unnamed action")).split())
        lines.append(f"  {event['dur'] / 1_000_000:.2f}s  {name}")
    if queues:
        p95 = queues[math.ceil(0.95 * len(queues)) - 1]
        lines.append(
            f"Remote queue: {len(queues)} events; p95 {p95:.2f}s; "
            f"max {queues[-1]:.2f}s (events may overlap)."
        )
    else:
        lines.append("Remote queue: no timing events in this profile.")
    return lines


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("profile", type=Path)
    parser.add_argument("--summary", type=Path, help="append to GitHub step summary")
    args = parser.parse_args()
    if not args.profile.is_file():
        print(f"Bazel profile unavailable: {args.profile}")
        return 0
    try:
        opener = gzip.open if args.profile.suffix == ".gz" else open
        with opener(args.profile, "rt", encoding="utf-8") as stream:
            lines = digest(json.load(stream))
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"::warning::Could not summarize Bazel profile: {error}", file=sys.stderr)
        return 0  # Diagnostics must not change the result of the build.
    print("\n".join(lines))
    if args.summary:
        with args.summary.open("a", encoding="utf-8") as stream:
            stream.write("### Bazel profile\n\n")
            stream.write("\n".join(f"- {line}" for line in lines))
            stream.write("\n\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
