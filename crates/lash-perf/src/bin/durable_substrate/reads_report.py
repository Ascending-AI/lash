#!/usr/bin/env python3
"""Render the declared-read ledger, retaining exact counts and nearest-rank timing."""
import argparse
from collections import defaultdict
import json
import gzip
from pathlib import Path
import statistics


def ranks(values):
    values = sorted(values)
    return [values[(len(values) * p + 99) // 100 - 1] / 1000 for p in (50, 95, 99)]


def times(values):
    return " / ".join(f"{v:.3f}" for v in ranks(values)) if values else "—"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inputs", type=Path, nargs="+", required=True)
    args = parser.parse_args()
    groups = defaultdict(list)
    for path in args.inputs:
        stream = gzip.open(path, "rt") if path.suffix == ".gz" else path.open()
        with stream:
            for line in stream:
                row = json.loads(line)
                if row.get("kind") == "read_sample":
                    key = (row["store"], row["dialect"], row["payload_bytes"], row["traced"], row["variant"])
                    groups[key].append(row)
    if not groups:
        parser.error("no read samples")
    print("| Store | Dialect | Body bytes | Traced | Variant | Samples | p50 / p95 / p99 ms | Plan commit p50 / p95 / p99 ms |")
    print("|---|---|---:|---|---|---:|---:|---:|")
    for key, rows in sorted(groups.items()):
        store, dialect, size, traced, variant = key
        structures = {(json.dumps(r["owner_commits"], sort_keys=True), json.dumps(r["run_facts"], sort_keys=True)) for r in rows}
        if len(structures) != 1:
            raise ValueError(f"{key}: structural counts changed within the population")
        print(f"| {store} | {dialect} | {size} | {traced} | {variant} | {len(rows)} | {times([r['elapsed_us'] for r in rows])} | {times([r['plan_commit_us'] for r in rows if r['plan_commit_us'] is not None])} |")
    print("\nPersisted bytes are newly inserted canonical graph-node payload bytes in accepted owner commits. They exclude SQL metadata, indexes, WAL and mailbox/maintenance writes. Frames count both directions in the enclosing send-to-outcome window.\n")
    print("| Store | Dialect | Body bytes | Traced | Variant | Installed bytes median (min–max) | IPC frames sent / received |")
    print("|---|---|---:|---|---|---:|---|")
    for key, rows in sorted(groups.items()):
        store, dialect, size, traced, variant = key
        sizes = [r['installed_graph_bytes'] for r in rows]
        frames = sorted({(r['ipc_sent_frames'], r['ipc_received_frames']) for r in rows})
        frame_text = "; ".join(f"{sent} / {received}" for sent, received in frames)
        print(f"| {store} | {dialect} | {size} | {traced} | {variant} | {statistics.median(sizes):.0f} ({min(sizes)}–{max(sizes)}) | {frame_text} |")
    print("\nExact accepted owner-commit labels and appended fact kinds (per sample):\n")
    structures = defaultdict(list)
    for key, rows in sorted(groups.items()):
        row = rows[0]
        structure = (json.dumps(row['owner_commits'], sort_keys=True), json.dumps(row['run_facts'], sort_keys=True))
        structures[structure].append('/'.join(str(v) for v in key))
    for (labels, facts), keys in structures.items():
        print(f"- `{', '.join(keys)}`: commits `{labels}`; facts `{facts}`.")


if __name__ == "__main__":
    main()
