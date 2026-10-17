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


IDENTITY_STRINGS = ("source", "compiler", "build_profile", "allocator", "host",
                    "executable", "worker", "backend", "configured_durability",
                    "observer", "window", "terminal")


def load_groups(paths):
    groups = defaultdict(list)
    refused = defaultdict(int)
    for path in paths:
        stream = gzip.open(path, "rt") if path.suffix == ".gz" else path.open()
        with stream:
            for line in stream:
                row = json.loads(line)
                if row.get("kind") != "read_sample":
                    continue
                if row.get("validation") != "passed":
                    refused["sample validation absent or failed"] += 1
                    continue
                identity = row.get("identity", {})
                missing = [key for key in IDENTITY_STRINGS
                           if not isinstance(identity.get(key), str) or not identity[key].strip()]
                if not isinstance(identity.get("dirty"), bool):
                    missing.append("dirty")
                if missing:
                    refused["missing identity: " + ", ".join(missing)] += 1
                    continue
                cohort = json.dumps(identity, sort_keys=True, separators=(",", ":"))
                key = (cohort, row["store"], row["dialect"], row["payload_bytes"], row["traced"], row["variant"])
                groups[key].append(row)
    return groups, dict(refused)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inputs", type=Path, nargs="+", required=True)
    args = parser.parse_args()
    groups, refused = load_groups(args.inputs)
    print("Excluded read samples: " + json.dumps(refused, sort_keys=True))
    if not groups:
        parser.error("no validated read samples with complete experiment identity")
    cohorts = defaultdict(dict)
    for (cohort, *population), rows in groups.items():
        cohorts[cohort][tuple(population)] = rows
    print(f"Admitted {sum(map(len, groups.values()))} samples in {len(cohorts)} distinct experiment cohorts; timing is never pooled across cohorts.")
    for cohort, populations in sorted(cohorts.items()):
        print("\nExperiment identity: `" + cohort + "`\n")
        render(populations)


def render(groups):
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


import unittest


class AdmissionTests(unittest.TestCase):
    def test_failed_or_incomplete_samples_cannot_enter_a_table(self):
        import contextlib
        import io
        import sys
        import tempfile
        from unittest.mock import patch
        row = dict(kind="read_sample", store="sqlite-file", dialect="typescript",
                   payload_bytes=32, traced=False, variant="read", elapsed_us=1,
                   plan_commit_us=None, owner_commits={}, run_facts={},
                   installed_graph_bytes=0, ipc_sent_frames=0, ipc_received_frames=0,
                   validation="failed")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "failed.jsonl"
            path.write_text(json.dumps(row) + "\n")
            with patch.object(sys, "argv", ["reads_report.py", "--inputs", str(path)]):
                with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    with self.assertRaises(SystemExit):
                        main()

    def test_source_build_host_and_observer_cohorts_never_pool(self):
        import tempfile
        identity = dict.fromkeys(IDENTITY_STRINGS, "control") | {"dirty": False}
        rows = [dict(kind="read_sample", store="sqlite-file", dialect="typescript",
                     payload_bytes=32, traced=False, variant="read", validation="passed",
                     identity=identity)]
        for field in IDENTITY_STRINGS:
            rows.append(rows[0] | {"identity": identity | {field: "different"}})
            rows.append(rows[0] | {"identity": identity | {field: None}})
        rows.append(rows[0] | {"validation": "failed"})
        rows.append(rows[0] | {"validation": None})
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "cohorts.jsonl"
            path.write_text("".join(json.dumps(row) + "\n" for row in rows))
            groups, refused = load_groups([path])
        self.assertEqual(len(groups), 1 + len(IDENTITY_STRINGS))
        self.assertTrue(all(len(rows) == 1 for rows in groups.values()))
        self.assertEqual(sum(refused.values()), len(IDENTITY_STRINGS) + 2)
        print("read-admission.json=" + json.dumps(dict(admitted_cohorts=len(groups),
              admitted_samples=sum(map(len, groups.values())), refused=refused), sort_keys=True))


if __name__ == "__main__":
    main()
