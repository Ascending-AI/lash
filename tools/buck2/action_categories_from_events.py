#!/usr/bin/env python3
"""Attribute the pool's per-action usage to Buck2 action categories.

The worker's usage log (`ACTION_USAGE_LOG`, see `action_sizes_from_log.py`)
records what an action used but not which Buck2 action it was: the supervisor
sees `python3` and a Cargo identity, and a target's metadata, Clippy, codegen
and link actions all share both. Buck2's own event log has the other half --
owner, category, identifier, the request, and when the worker ran it -- but no
usage. This joins the two for the invocations of one checkout:

    buck2 --isolation-dir kiln log show <events.pb.zst> > events.jsonl
    python3 tools/buck2/action_categories_from_events.py \\
      --events events.jsonl usage-*.log

`buck2 --isolation-dir kiln log path --all` lists the event logs. An executed
(not cached) remote action matches the one usage record with the same
`memory_kb` request that ended within three seconds of it and ran as long, to
within 0.4 s or 6%. An action with no such record, or with several, is left
out: its worker's log was not given, or the match would be a guess.

The table is evidence for sizing decisions, not an input to the size files:
Buck2 resolves one execution platform per target, so every category of a
target shares the target's request.
"""

from __future__ import annotations

import argparse
import bisect
import collections
import json
import pathlib
import sys

from action_sizes_from_log import as_int, parse_record, percentile

END_TOLERANCE_SECONDS = 3
WALL_TOLERANCE_SECONDS = 0.4
WALL_TOLERANCE_RATIO = 0.06
TARGET_KINDS = {"rust_test": "test binary", "rust_binary": "binary", "rust_library": "library"}


def executed_actions(lines):
    """Every remote, uncached command of the event log's actions."""
    for line in lines:
        if '"ActionExecution"' not in line or '"SpanEnd"' not in line:
            continue
        action = json.loads(line)["Event"]["data"]["SpanEnd"]["data"]["ActionExecution"]
        for command in action.get("commands") or []:
            details = command["details"]
            remote = (details.get("command_kind") or {}).get("command", {}).get("RemoteCommand")
            if not remote or remote.get("cache_hit"):
                continue
            properties = {
                item["name"]: item["value"]
                for item in remote["details"]["platform"]["properties"]
            }
            metadata = details["metadata"]
            seconds, nanoseconds = metadata["start_time"]
            wall = metadata["execution_time_us"] / 1e6
            category = action["name"]["category"]
            # `rustc` identifiers read `<emit> [pic] ...`; the emit is the kind.
            emit = action["name"].get("identifier", "").split(" ")[0] if category == "rustc" else ""
            label = action["key"]["owner"].get("TargetLabel", {}).get("label", {})
            kind = TARGET_KINDS.get(action.get("target_rule_type_name"), "other")
            if "third-party" in label.get("package", ""):
                kind = "third-party"
            yield {
                "group": (category, emit, kind),
                "end": seconds + nanoseconds / 1e9 + wall,
                "wall": wall,
                "memory_kb": as_int(properties.get("memory_kb")) or 0,
            }


def usage_records(lines):
    records = []
    for line in lines:
        record = parse_record(line)
        stamp = line.partition("\t")[0]
        if record is None or not stamp.isdigit():
            continue
        fields = [
            as_int(record.get(name))
            for name in ("wall_ms", "cpu_usec", "peak_bytes", "requested_kb")
        ]
        if None in fields:
            continue
        records.append((int(stamp), *fields))
    return sorted(records)


def join(actions, records):
    """Per group: (peak bytes, wall seconds, cpu seconds, requested KiB) of each match."""
    stamps = [record[0] for record in records]
    matched = collections.defaultdict(list)
    for action in actions:
        low = bisect.bisect_left(stamps, action["end"] - END_TOLERANCE_SECONDS)
        high = bisect.bisect_right(stamps, action["end"] + END_TOLERANCE_SECONDS)
        tolerance = max(WALL_TOLERANCE_SECONDS, WALL_TOLERANCE_RATIO * action["wall"])
        candidates = [
            record
            for record in records[low:high]
            if record[4] == action["memory_kb"]
            and abs(record[1] / 1000 - action["wall"]) <= tolerance
        ]
        if len(candidates) != 1:
            continue
        _stamp, wall_ms, cpu_usec, peak_bytes, requested_kb = candidates[0]
        matched[action["group"]].append((peak_bytes, wall_ms / 1000, cpu_usec / 1e6, requested_kb))
    return matched


def render(matched) -> str:
    out = [
        "| category | emit | target | actions | wall s | peak MiB p50 | p95 | max | cores | memory x |",
        "| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for group, rows in sorted(matched.items(), key=lambda item: -sum(row[1] for row in item[1])):
        peaks = [row[0] for row in rows]
        wall = sum(row[1] for row in rows)
        reserved = sum(row[3] * 1024 * row[1] for row in rows)
        used = sum(row[0] * row[1] for row in rows)
        out.append(
            f"| {group[0]} | {group[1] or '-'} | {group[2]} | {len(rows)} | {wall:.0f} "
            f"| {percentile(peaks, 50) >> 20} | {percentile(peaks, 95) >> 20} | {max(peaks) >> 20} "
            f"| {sum(row[2] for row in rows) / wall if wall else 0:.2f} "
            f"| {reserved / used if used else 0:.1f} |"
        )
    return "\n".join(out) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("logs", nargs="+", type=pathlib.Path, help="pool usage logs")
    parser.add_argument(
        "--events",
        action="append",
        required=True,
        type=pathlib.Path,
        help="`buck2 log show` output of one invocation; repeatable",
    )
    args = parser.parse_args()
    actions = []
    for path in args.events:
        with path.open(encoding="utf-8") as lines:
            actions.extend(executed_actions(lines))
    records = usage_records(
        line
        for path in args.logs
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines()
    )
    matched = join(actions, records)
    sys.stdout.write(render(matched))
    print(
        f"{sum(len(rows) for rows in matched.values())} of {len(actions)} executed actions matched",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
