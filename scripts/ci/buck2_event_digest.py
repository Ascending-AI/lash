#!/usr/bin/env python3
"""Decode an existing Buck2 log into build-cost.json and its full critical path."""

from __future__ import annotations

import argparse
from collections import Counter, defaultdict
import hashlib
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools/buck2"))
from action_categories_from_events import digest_key, target_identities  # noqa: E402
from action_sizes_from_log import as_int, held_bytes, parse_record  # noqa: E402


# Keep the complete Buck payloads, including fields unknown to this reader.
IDENTITY_KINDS = {
    "ConfigurationCreated", "VersionControlRevision", "CommandOptions",
    "ReSession", "SystemInfo", "BuildGraphInfo",
}


def client() -> str:
    result = subprocess.run(
        [sys.executable, "tools/buck2/bootstrap.py", "--client-only"],
        cwd=ROOT, capture_output=True, text=True, check=True,
    )
    executable = result.stdout.strip()
    if not executable:
        raise RuntimeError("Buck2 bootstrap returned no executable")
    return executable


def command(executable: str, *args: str) -> str:
    result = subprocess.run(
        [executable, "log", *args], cwd=ROOT,
        capture_output=True, text=True, check=True,
    )
    return result.stdout.rstrip("\n")


def file_identity(path: Path) -> dict:
    with path.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    return {"path": str(path.resolve()), "sha256": digest}


def timestamp_ns(event: dict | None) -> int | None:
    if event is None or event.get("timestamp") is None:
        return None
    seconds, nanos = event["timestamp"]
    return seconds * 1_000_000_000 + nanos


def payload(event: dict | None, endpoint: str, kind: str) -> dict:
    return (event or {}).get("data", {}).get(endpoint, {}).get("data", {}).get(kind, {})


def read_events(lines) -> dict:
    """One pass; span parents are resolved with memoization after decoding."""
    spans = {}
    parents = {}
    counts = Counter()
    identities = defaultdict(list)
    traces = set()
    records = []
    timeline = []
    event_count = 0
    for line in lines:
        record = json.loads(line)
        if "Event" not in record:
            records.append(record)
            continue
        event = record["Event"]
        event_count += 1
        traces.add(event["trace_id"])
        data = event["data"]
        endpoint = next(iter(data))
        inner = data[endpoint].get("data", {})
        kind = next(iter(inner), "")
        counts[f"{endpoint}/{kind}"] += 1
        span_id = event.get("span_id")
        if endpoint == "SpanStart":
            parents[span_id] = event.get("parent_id")
        if endpoint in ("SpanStart", "SpanEnd"):
            span = spans.setdefault(span_id, {"span_id": span_id, "kind": kind})
            span[endpoint] = event
            if kind == "ActionExecution":
                timeline.append({"timestamp_ns": timestamp_ns(event), "span_id": span_id,
                                 "boundary": "start" if endpoint == "SpanStart" else "end"})
        if endpoint == "Instant" and kind in IDENTITY_KINDS:
            identities[kind].append(event)
    if event_count == 0 or len(traces) != 1:
        raise ValueError("expected events from exactly one Buck invocation")
    for span in spans.values():
        span["missing_endpoints"] = [key for key in ("SpanStart", "SpanEnd") if key not in span]
    actions = [span for span in spans.values() if span["kind"] == "ActionExecution"]
    action_ids = {span["span_id"] for span in actions}
    owners = {span_id: span_id for span_id in action_ids}
    owners[0] = None
    owners[None] = None

    def owner(span_id):
        trail = []
        while span_id not in owners:
            trail.append(span_id)
            # A corrupt parent cycle must not loop forever.
            owners[span_id] = None
            span_id = parents.get(span_id)
        result = owners[span_id]
        for item in trail:
            owners[item] = result
        return result

    for span in spans.values():
        span["action_span_id"] = owner(span["span_id"])
        span["action_join"] = "exact_span_ancestry" if span["action_span_id"] is not None else "missing_action_ancestor"
    return {
        "trace_id": next(iter(traces)), "event_count": event_count,
        "event_counts": dict(counts), "log_records": records,
        "identity_events": dict(identities), "spans": list(spans.values()),
        # Interval endpoints retain all overlaps without quadratic action pairs
        # or sorting. No wall-time sum is presented as command elapsed time.
        "action_intervals": timeline,
        "overlap_unit": "daemon timestamp_ns interval endpoints; intervals may overlap",
    }


def scheduler_usage(usage_lines, trace_lines, priority_valid_from=None):
    """Bridge exact operation IDs; retain dispatch attempts in capture order.

    Counts must agree and both streams must already be chronological per
    operation. Refuse ambiguous/truncated retry populations rather than sorting
    or matching by reservation, duration or timestamp proximity.
    """
    records = []
    operations = defaultdict(list)
    for line in usage_lines:
        record = parse_record(line)
        if record is None:
            continue
        record["_capture_end_s"] = line.partition("\t")[0]
        records.append(record)
        if (all(as_int(record.get(key)) is not None for key in ("wall_ms", "cpu_usec", "requested_kb"))
                and held_bytes(record) is not None):
            operations[record.get("operation_id", record.get("op"))].append(record)
    dispatches = defaultdict(list)
    population = 0
    first = last = None
    for line in trace_lines:
        row = json.loads(line)
        if "repo:lash" not in row["identity"].split("|"):
            continue
        population += 1
        stamp = row["dispatch_s"]
        first = stamp if first is None else min(first, stamp)
        last = stamp if last is None else max(last, stamp)
        if row["operation_id"] not in operations:
            continue
        digest, separator, size = row["action_digest"].rpartition("-")
        if not separator or not size.isdigit() or not digest:
            raise ValueError("invalid scheduler action_digest (expected hash-size)")
        row = dict(row, action_digest=digest_key(digest + ":" + size))
        row["reported_effective_priority"] = row.pop("effective_priority", None)
        row["effective_priority"] = (row["reported_effective_priority"]
                                     if priority_valid_from is not None and stamp >= priority_valid_from else None)
        dispatches[row["operation_id"]].append(row)
    statuses = Counter()
    for operation, attempts in operations.items():
        rows = dispatches.get(operation, [])
        status = "missing_scheduler_operation"
        if rows:
            ordered_dispatch = all(a["dispatch_s"] <= b["dispatch_s"] for a, b in zip(rows, rows[1:]))
            stamps = [as_int(r["_capture_end_s"]) for r in attempts]
            ordered_usage = None not in stamps and all(a <= b for a, b in zip(stamps, stamps[1:]))
            attempt_ids = [r.get("attempt") for r in attempts]
            unique_attempts = (len(attempts) == 1 or
                               (None not in attempt_ids and len(set(attempt_ids)) == len(attempt_ids)))
            if len(rows) == len(attempts) and ordered_dispatch and ordered_usage and unique_attempts:
                status = "exact_operation_attempt_order"
            else:
                status = "ambiguous_operation_attempts"
        statuses[status] += len(attempts)
        for index, record in enumerate(attempts):
            record["_scheduler_join"] = status
            if status == "exact_operation_attempt_order":
                row = rows[index]
                existing = record.get("action_digest")
                if existing not in (None, "", "-") and digest_key(existing) != row["action_digest"]:
                    raise ValueError("worker and scheduler action digests disagree")
                record["action_digest"] = row["action_digest"]
                record["_scheduler"] = dict(row, attempt_index=index, attempt_count=len(attempts))
    return records, {"dispatch_rows": population, "dispatch_first_s": first, "dispatch_last_s": last,
                     "priority_valid_from_s": priority_valid_from, "usage_attempt_joins": dict(statuses)}


def join_usage(receipt: dict, usage_lines, inventory: dict) -> None:
    """Only a unique digest/category/Cargo identity may supply resource usage.

    The sizing reader's legacy reservation/time fallback is deliberately absent:
    it cannot identify retries, concurrent identical actions or cache hits.
    """
    identities = target_identities(inventory)
    records = defaultdict(list)
    usage_count = 0
    missing_digest = 0
    incomplete_usage = 0
    for line in usage_lines:
        record = line if isinstance(line, dict) else parse_record(line)
        if record is None:
            continue
        usage_count += 1
        digest = record.get("action_digest")
        if not digest or digest == "-":
            missing_digest += 1
            continue
        if (any(as_int(record.get(key)) is None for key in ("wall_ms", "cpu_usec", "requested_kb"))
                or held_bytes(record) is None):
            incomplete_usage += 1
            continue
        records[digest_key(digest)].append(record)
    attempts = []
    digest_counts = Counter()
    for span in receipt["spans"]:
        if span["kind"] != "ActionExecution":
            continue
        action = payload(span.get("SpanEnd"), "SpanEnd", "ActionExecution")
        owner = action.get("key", {}).get("owner", {}).get("TargetLabel", {})
        label = owner.get("label", {})
        # Same generated-label normalization as the action-category reader.
        name = label.get("name", "").removesuffix("__metadata").removesuffix("__rust_test")
        target = "//{}:{}".format(label.get("package", "").partition("//")[2], name)
        identity = identities.get(target)
        span["cargo_identity"] = list(identity) if identity else None
        span["usage"] = []
        for index, item in enumerate(action.get("commands") or []):
            details = item.get("details", {})
            kinds = (details.get("command_kind") or {}).get("command", {})
            remote = kinds.get("RemoteCommand")
            # Cache resource statistics describe the original producer, not
            # work done in this invocation.
            eligible = remote is not None and remote.get("cache_hit") is False
            digest = digest_key(remote.get("action_digest")) if remote else None
            row = {"command_index": index, "action_digest": digest,
                   "status": "missing_exact_usage", "cpu_usec": None,
                   "rss_peak_bytes": None, "cgroup_peak_bytes": None,
                   "anonymous_peak_bytes": None, "record": None}
            if not eligible:
                row["status"] = "cache_hit_or_not_remote_execution"
            elif not digest:
                row["status"] = "missing_action_digest"
            elif identity is None:
                row["status"] = "missing_cargo_identity"
            if eligible and digest:
                digest_counts[digest] += 1
            span["usage"].append(row)
            attempts.append((row, identity, action.get("name", {}).get("category")))
    for row, identity, category in attempts:
        if row["status"] != "missing_exact_usage":
            continue
        digest = row["action_digest"]
        candidates = records.get(digest, [])
        scheduler_rows = [r.get("_scheduler", {}) for r in candidates]
        retry_group = (len(candidates) > 1 and all(scheduler_rows)
                       and len({r["operation_id"] for r in scheduler_rows}) == 1
                       and all(r["attempt_count"] == len(candidates) for r in scheduler_rows))
        if (len(candidates) > 1 and not retry_group) or digest_counts[digest] != 1:
            row["status"] = "ambiguous_action_attempt"
        elif candidates:
            if any(record.get("category") != category or f"{record.get('pkg')}/{record.get('crate')}" != identity[0]
                   for record in candidates):
                row["status"] = "usage_identity_mismatch"
                continue
            row["worker_attempts"] = candidates
            if retry_group:
                row["status"] = "exact_scheduler_operation_attempts"
                continue
            record = candidates[0]
            row.update(status="exact_action_digest", record=record,
                       cpu_usec=as_int(record.get("cpu_usec")),
                       cgroup_peak_bytes=as_int(record.get("peak_bytes")),
                       anonymous_peak_bytes=as_int(record.get("anon_peak_bytes")))
            # ACTION_USAGE_LOG reports cgroup/anonymous peaks, not RSS.
    receipt["usage_population"] = {"records": usage_count, "without_digest": missing_digest,
                                   "incomplete_samples": incomplete_usage,
                                   "joins": dict(Counter(row["status"] for row, _, _ in attempts))}


def enrich(receipt: dict) -> None:
    by_id = {span["span_id"]: span for span in receipt["spans"]}
    graphs = receipt["identity_events"].get("BuildGraphInfo", [])
    critical_joins = []
    for graph_event in graphs:
        graph = payload(graph_event, "Instant", "BuildGraphInfo")
        for index, entry in enumerate(graph.get("critical_path2", [])):
            ids = entry.get("span_ids") or []
            critical_joins.append({"entry_index": index, "span_ids": ids,
                                   "missing_span_ids": [item for item in ids if item not in by_id],
                                   "join": "exact_span_ids" if ids else "no_span_ids_reused_or_synthetic"})
    receipt["critical_path_joins"] = critical_joins
    receipt["critical_path_status"] = "present" if graphs else "missing_build_graph_info"
    receipt["commands"] = [
        {"span_id": span["span_id"],
         "elapsed_us": (span.get("SpanEnd") or {}).get("data", {}).get("SpanEnd", {}).get("duration_us"),
         "terminal": payload(span.get("SpanEnd"), "SpanEnd", "Command") or None}
        for span in receipt["spans"] if span["kind"] == "Command"
    ]
    receipt["population"] = {"actions": sum(s["kind"] == "ActionExecution" for s in receipt["spans"]),
                             "executor_stages": sum(s["kind"] == "ExecutorStage" for s in receipt["spans"]),
                             "materializations": sum(s["kind"] == "Materialization" for s in receipt["spans"])}
    receipt["limitations"] = [
        "Only observed events; reused actions may have no execution span in this invocation.",
        "Missing endpoints, ancestry, critical-path span IDs and usage remain explicit; absent is not zero.",
        "Action/queue/execution/materialization intervals overlap; never sum them into elapsed time.",
        "rustc link includes compile and link work; linker-only time requires compiler self-profile evidence.",
        "Worker cgroup and anonymous peaks are not RSS; RSS is unavailable in this usage format.",
        "No qualified clean/edit comparison without external edit provenance and matching build/observer/terminal identities.",
    ]


def action_table(receipt: dict) -> list[str]:
    """All actions, in event order; IDs join to complete stage/command payloads."""
    lines = ["Actions (one row per command attempt, or commandless action; us, bytes; '-' means unavailable):",
             "span\towner/configuration\tcategory\tidentifier\taction_span_us\tcommand_execution_us\tqueue_us\tcache_hit\tcpu_usec\tcgroup_peak_bytes\tusage_join"]
    for span in receipt["spans"]:
        if span["kind"] != "ActionExecution":
            continue
        action = payload(span.get("SpanEnd"), "SpanEnd", "ActionExecution") or payload(span.get("SpanStart"), "SpanStart", "ActionExecution")
        owner = action.get("key", {}).get("owner")
        name = action.get("name", {})
        base = [span["span_id"], json.dumps(owner, separators=(",", ":")),
                name.get("category"), name.get("identifier"),
                (span.get("SpanEnd") or {}).get("data", {}).get("SpanEnd", {}).get("duration_us")]
        commands = action.get("commands") or []
        for index in range(max(1, len(commands))):
            details = commands[index].get("details", {}) if commands else {}
            metadata = details.get("metadata") or {}
            remote = (details.get("command_kind") or {}).get("command", {}).get("RemoteCommand", {})
            usage = span["usage"][index] if commands else {}
            values = base + [metadata.get("execution_time_us"), metadata.get("queue_duration_us"),
                             remote.get("cache_hit"), usage.get("cpu_usec"), usage.get("cgroup_peak_bytes"),
                             usage.get("status", "no_command")]
            lines.append("\t".join("-" if v is None else str(v) for v in values))
            for attempt in usage.get("worker_attempts", []):
                lines.append(f"worker_attempt\t{span['span_id']}\tcommand={index}\t" + json.dumps(attempt, separators=(",", ":")))
    return lines


def digest(event_log: Path, out_dir: Path | None = None, usage_logs=(), scheduler_trace=None, priority_valid_from=None) -> list[str]:
    executable = client()
    summary = command(executable, "summary", str(event_log))
    critical = command(executable, "critical-path", "--format", "readable", str(event_log))
    receipt = read_events(command(executable, "show", str(event_log)).splitlines())
    receipt["source"] = file_identity(event_log)
    receipt["observer"] = {"script": file_identity(Path(__file__)), "decoder": file_identity(Path(executable)),
                           "mode": "offline_build_cost", "usage_logs": [file_identity(p) for p in usage_logs]}
    inventory_path = ROOT / "tools/buck2/target-inventory.json"
    receipt["observer"]["target_inventory"] = file_identity(inventory_path)
    inventory = json.loads(inventory_path.read_text(encoding="utf-8"))
    def usage_lines():
        for path in usage_logs:
            with path.open(encoding="utf-8") as stream:
                yield from stream
    lines_to_join = usage_lines()
    if scheduler_trace is not None:
        receipt["observer"]["scheduler_trace"] = file_identity(scheduler_trace)
        with scheduler_trace.open(encoding="utf-8") as stream:
            lines_to_join, coverage = scheduler_usage(lines_to_join, stream, priority_valid_from)
        starts = [timestamp_ns(s.get("SpanStart")) for s in receipt["spans"] if s["kind"] == "Command"]
        ends = [timestamp_ns(s.get("SpanEnd")) for s in receipt["spans"] if s["kind"] == "Command"]
        coverage["command_window"] = ("covered" if starts and ends and None not in starts + ends
            and coverage["dispatch_first_s"] is not None
            and coverage["dispatch_first_s"] <= min(starts) / 1e9
            and max(ends) / 1e9 <= coverage["dispatch_last_s"] else "uncovered_or_partial")
        receipt["scheduler_coverage"] = coverage
    join_usage(receipt, lines_to_join, inventory)
    enrich(receipt)
    receipt["summary_text"] = summary
    receipt["critical_path_text"] = critical
    destination = out_dir or event_log.parent / (event_log.name + ".build-cost")
    destination.mkdir(parents=True, exist_ok=True)
    (destination / "build-cost.json").write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
    (destination / "critical-path.txt").write_text(critical + "\n", encoding="utf-8")
    lines = [
        f"Build cost: {destination / 'build-cost.json'}",
        f"Invocation: {receipt['trace_id']}; events={receipt['event_count']}; {receipt['population']}",
        f"Usage joins: {receipt['usage_population']}",
        f"Scheduler coverage: {receipt.get('scheduler_coverage', 'not supplied')}",
        *(f"Command span {c['span_id']}: elapsed_us={c['elapsed_us']} terminal={c['terminal']} (daemon command interval)" for c in receipt['commands']),
        *receipt["limitations"],
        "Buck2 summary:", *(f"  {line}" for line in summary.splitlines()),
        "Critical path:", *(f"  {line}" for line in critical.splitlines()),
        *action_table(receipt),
    ]
    (destination / "build-cost.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")
    return lines


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("event_log", type=Path)
    parser.add_argument("--summary", type=Path, help="append full text to GitHub step summary")
    parser.add_argument("--out-dir", type=Path, help="default: <event_log>.build-cost/")
    parser.add_argument("--usage-log", type=Path, action="append", default=[], help="captured worker ACTION_USAGE_LOG; repeat per worker")
    parser.add_argument("--scheduler-trace", type=Path, help="offline scheduler dispatch JSONL export; exact operation_id bridge")
    parser.add_argument("--scheduler-priority-valid-from", type=float, help="export provenance: Unix seconds after which priorities are trustworthy; default unavailable")
    args = parser.parse_args()
    try:
        if not args.event_log.is_file():
            raise ValueError(f"Buck2 event log unavailable: {args.event_log}")
        lines = digest(args.event_log, args.out_dir, args.usage_log, args.scheduler_trace, args.scheduler_priority_valid_from)
        print("\n".join(lines))
        if args.summary:
            with args.summary.open("a", encoding="utf-8") as stream:
                stream.write("### Buck2 build event log\n\n```text\n")
                stream.write("\n".join(lines))
                stream.write("\n```\n\n")
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"::error::Could not decode Buck2 event log: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
