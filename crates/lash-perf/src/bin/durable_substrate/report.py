#!/usr/bin/env python3
"""Tabulate durable-substrate JSON-lines reports against the L12a baseline.

    report.py --inputs docs/perf/durable-substrate-2026-10.jsonl

Prints the Markdown tables of L12b's comparison document in docs/perf/.
Percentiles are nearest-rank over the pooled samples of each row.
"""
from __future__ import annotations

import argparse
import collections
import json
import math
from pathlib import Path

BASELINE_ROUNDS = {1: (119.874, 167.125, 360.789, 476.609), 5: (105.579, 150.994, 765.005, 963.034),
                   20: (101.828, 139.915, 2319.040, 2805.029)}
BASELINE_WRITES = {1: 92.60, 5: 125.60, 20: 272.90}
BASELINE_RESUME = {1: 332.183, 5: 447.078, 20: 1054.000}
BASELINE_PROCESS = 511.719
BASELINE_CONCURRENT = {1: (1171.587, 1474.448, 178.694, 248.142, 0.85),
                       10: (1668.896, 1958.159, 260.050, 556.099, 5.81),
                       100: (11611.930, 13019.464, 1784.890, 2359.189, 8.46)}
S2 = {("claim_4096_batch_1", 1): (2.990, 4.480), ("claim_4096_batch_16", 1): (3.530, 5.325),
      ("claim_4096_batch_64", 1): (4.092, 5.536), ("claim_16_batch_16", 1): (0.509, 0.814),
      ("fence_distinct", 1): (0.361, 0.531), ("heartbeat_reap_empty", 1): (0.218, 0.292),
      ("claim_4096_batch_1", 4): (2.882, 5.127), ("claim_4096_batch_16", 4): (3.788, 6.602),
      ("claim_4096_batch_64", 4): (4.815, 8.693), ("claim_16_batch_16", 4): (0.315, 0.718),
      ("fence_distinct", 4): (0.567, 0.740), ("heartbeat_reap_empty", 4): (0.320, 0.434),
      ("claim_4096_batch_1", 16): (5.163, 7.181), ("claim_4096_batch_16", 16): (8.143, 11.225),
      ("claim_4096_batch_64", 16): (12.262, 31.732), ("claim_16_batch_16", 16): (0.884, 1.120),
      ("fence_distinct", 16): (2.220, 2.488), ("heartbeat_reap_empty", 16): (1.282, 1.664),
      ("wake_notify_after_commit", 1): (0.636, 0.856), ("wake_notify_after_commit", 4): (0.658, 1.045),
      ("wake_notify_after_commit", 16): (0.760, 1.230)}
LEASE = ("node.claim", "node.heartbeat", "node.reap", "node.register", "node.release")


def rank(values, percent):
    ordered = sorted(values)
    return ordered[math.ceil(len(ordered) * percent / 100) - 1]


def ms(us):
    return us / 1000.0


def where(store, nodes):
    return f"{'SQLite file' if store == 'sqlite-file' else 'PostgreSQL'}{'' if store == 'sqlite-file' else f' x{nodes}'}"


def load(paths):
    rows = []
    for path in paths:
        for line in Path(path).read_text().splitlines():
            if line.strip():
                rows.append(json.loads(line))
    return rows


def groups(rows, kind, case_prefix):
    grouped = collections.defaultdict(list)
    for row in rows:
        if row["kind"] == kind and row["case"].startswith(case_prefix):
            grouped[(row["dialect"], row.get("nodes", 1), row["case"])].append(row)
    return grouped


def ratio(base, value):
    return f"{base / value:.1f}x" if value else "n/a"


def rounds_table(rows):
    print("| Store | Tool rounds | Turns | Round p50 | Round p99 | Total p50 | Total p99 | Baseline round p50 / p99 | Round p50 / p99 lower by |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|---:|")
    for (store, nodes, case), turns in sorted(groups(rows, "turn", "rounds-").items(),
                                               key=lambda item: (item[0][0], item[0][1], int(item[0][2].split("-")[1]))):
        count = int(case.split("-")[1])
        rounds = [r for t in turns for r in t["round_us"]]
        totals = [t["total_us"] for t in turns]
        base = BASELINE_ROUNDS[count]
        p50, p99 = ms(rank(rounds, 50)), ms(rank(rounds, 99))
        print(f"| {where(store, nodes)} | {count} | {len(turns)} | {p50:.3f} | {p99:.3f} | "
              f"{ms(rank(totals, 50)):.3f} | {ms(rank(totals, 99)):.3f} | {base[0]:.3f} / {base[1]:.3f} | "
              f"{ratio(base[0], p50)} / {ratio(base[1], p99)} |")


def writes_table(rows):
    print("| Store | Rounds | Durable txns/turn | of which lease/claim | PG write statements/turn | PG write rows/turn | PG statements/turn (all) | WAL KiB/turn | SQLite KiB/turn | Checkpoint KiB/turn | Baseline SQL statements/turn |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    for (store, nodes, case), batches in sorted(groups(rows, "batch", "rounds-").items(),
                                                 key=lambda item: (item[0][0], item[0][1], int(item[0][2].split("-")[1]))):
        count = int(case.split("-")[1])
        turns = sum(b["sessions"] for b in batches)
        txns = sum(b["durable_transactions"] for b in batches) / turns
        lease = sum(v for b in batches for k, v in b["transactions"].items() if k in LEASE) / turns
        pg = [b["counters"]["postgres"] for b in batches if b["counters"]["postgres"]]
        sqlite = [b["counters"]["sqlite_bytes"] for b in batches if b["counters"]["sqlite_bytes"] is not None]
        ckpt = sum(b["checkpoint_bytes"] for b in batches) / turns / 1024
        cell = lambda key: f"{sum(p[key] for p in pg) / turns:.2f}" if pg else "-"
        wal = f"{sum(p['wal_bytes'] for p in pg) / turns / 1024:.1f}" if pg else "-"
        sq = f"{sum(sqlite) / turns / 1024:.1f}" if sqlite and not pg else "-"
        print(f"| {where(store, nodes)} | {count} | {txns:.2f} | {lease:.2f} | {cell('write_statements')} | "
              f"{cell('write_rows')} | {cell('all_statements')} | {wal} | {sq} | {ckpt:.1f} | {BASELINE_WRITES[count]:.2f} |")


def labels_table(rows):
    print("| Store | Rounds | Owner and mail transactions per turn, by label |")
    print("|---|---:|---|")
    for (store, nodes, case), batches in sorted(groups(rows, "batch", "rounds-").items(),
                                                 key=lambda item: (item[0][0], item[0][1], int(item[0][2].split("-")[1]))):
        turns = sum(b["sessions"] for b in batches)
        totals = collections.Counter()
        for b in batches:
            totals.update({k: v for k, v in b["transactions"].items() if k not in LEASE})
        text = ", ".join(f"`{k}` {v / turns:g}" for k, v in sorted(totals.items()))
        print(f"| {where(store, nodes)} | {case.split('-')[1]} | {text} |")


def concurrent_table(rows):
    print("| Store | Sessions | Batches | Completed turns | Total p50 ms | Total p99 ms | Round p50 ms | Round p99 ms | Turns/s | Baseline total p99 / turns/s |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    batches = groups(rows, "batch", "concurrent-")
    for (store, nodes, case), turns in sorted(groups(rows, "turn", "concurrent-").items(),
                                               key=lambda item: (item[0][0], item[0][1], int(item[0][2].split("-")[1]))):
        sessions = int(case.split("-")[1])
        rounds = [r for t in turns for r in t["round_us"]]
        totals = [t["total_us"] for t in turns]
        window = sum(b["window_us"] for b in batches[(store, nodes, case)]) / 1e6
        base = BASELINE_CONCURRENT[sessions]
        print(f"| {where(store, nodes)} | {sessions} | {len(batches[(store, nodes, case)])} | {len(turns)} | "
              f"{ms(rank(totals, 50)):.3f} | {ms(rank(totals, 99)):.3f} | {ms(rank(rounds, 50)):.3f} | "
              f"{ms(rank(rounds, 99)):.3f} | {len(turns) / window:.2f} | {base[1]:.3f} / {base[4]:.2f} |")


def resume_table(rows, prefix):
    print("| Store | Case | Rounds before hold | Prior turns | Samples | Checkpoint KiB (first / last) | Claim to commit p50 / max ms | Fresh boot to commit p50 / max ms | Resumed `turn.commit` p50 ms |")
    print("|---|---|---:|---|---:|---:|---:|---:|---:|")
    for (store, nodes, case), samples in sorted(groups(rows, "resume", prefix).items(),
                                                 key=lambda item: (item[0][0], int(item[0][2].split("-")[1]))):
        prior = f"{samples[0]['prior_turns']}–{samples[-1]['prior_turns']}"
        claim = [s["claim_to_commit_us"] for s in samples]
        boot = [s["boot_to_commit_us"] for s in samples]
        print(f"| {where(store, nodes)} | {case} | {samples[0]['rounds_before_hold']} | {prior} | {len(samples)} | "
              f"{samples[0]['checkpoint_bytes'] / 1024:.1f} / {samples[-1]['checkpoint_bytes'] / 1024:.1f} | "
              f"{ms(rank(claim, 50)):.1f} / {ms(max(claim)):.1f} | {ms(rank(boot, 50)):.1f} / {ms(max(boot)):.1f} | "
              f"{ms(rank([s['commit_us'] for s in samples], 50)):.1f} |")


def parked_table(rows):
    print("| Store | Samples | Pin to release ms | Parked s | Resolve commit p50 ms | Resolve to engine p50 / max ms | Resolve to `process.terminal` p50 / max ms | Baseline completion to outcome |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|")
    for (store, nodes, case), samples in sorted(groups(rows, "parked", "parked").items()):
        res = [s["resolve_to_resumed_us"] for s in samples]
        term = [s["resolve_to_terminal_us"] for s in samples]
        print(f"| {where(store, nodes)} | {len(samples)} | {ms(rank([s['release_after_pin_us'] for s in samples], 50)):.1f} | "
              f"{samples[0]['parked_us'] / 1e6:.1f} | {ms(rank([s['resolve_commit_us'] for s in samples], 50)):.2f} | "
              f"{ms(rank(res, 50)):.1f} / {ms(max(res)):.1f} | {ms(rank(term, 50)):.1f} / {ms(max(term)):.1f} | {BASELINE_PROCESS:.1f} |")
    print()
    print("| Store | Resolutions | Hot resolve to engine p50 / p99 ms |")
    print("|---|---:|---:|")
    for (store, nodes, case), samples in sorted(groups(rows, "wait", "process-waits").items()):
        lat = [s["resolve_to_resumed_us"] for s in samples]
        print(f"| {where(store, nodes)} | {len(lat)} | {ms(rank(lat, 50)):.3f} / {ms(rank(lat, 99)):.3f} |")


def cell_table(rows):
    print("| Store | Case | Blocks | Snapshot bytes p50 / max | Snapshot commit p50 / p99 ms | Block cycle p50 / p99 ms | Turn total p50 ms |")
    print("|---|---|---:|---:|---:|---:|---:|")
    batches = groups(rows, "batch", "cell-")
    for (store, nodes, case), blocks in sorted(groups(rows, "block", "cell-").items(),
                                                key=lambda item: (item[0][0], int(item[0][2].split("-")[1]))):
        size = [b["snapshot_bytes"] for b in blocks]
        commit = [b["commit_us"] for b in blocks]
        cycle = [b["cycle_us"] for b in blocks if b["cycle_us"] is not None]
        total = [b["window_us"] for b in batches[(store, nodes, case)]]
        print(f"| {where(store, nodes)} | {case} | {len(blocks)} | {rank(size, 50):,} / {max(size):,} | "
              f"{ms(rank(commit, 50)):.3f} / {ms(rank(commit, 99)):.3f} | {ms(rank(cycle, 50)):.3f} / {ms(rank(cycle, 99)):.3f} | "
              f"{ms(rank(total, 50)):.1f} |")


def idle_table(rows):
    print("| Store | Waiting actors | Window s | Durable txns/s | by label (per window) | PG statements/s | PG WAL KiB/s | Connections | Stored KiB per waiting actor |")
    print("|---|---:|---:|---:|---|---:|---:|---:|---:|")
    for row in sorted((r for r in rows if r["kind"] == "idle"), key=lambda r: (r["dialect"], r["nodes"], r["waiting_actors"])):
        pg = row["counters"]["postgres"]
        per = row["relation_bytes_per_waiting_actor"] or row["sqlite_bytes_per_waiting_actor"]
        labels = ", ".join(f"`{k}` {v}" for k, v in sorted(row["transactions"].items()))
        statements = f"{row['statements_per_s']:.1f}" if pg else "-"
        wal = f"{pg['wal_bytes'] / 1024 / row['window_s']:.2f}" if pg else "-"
        connections = row["connections"] if pg else "-"
        stored = f"{per / 1024:.2f}" if per else "-"
        print(f"| {where(row['dialect'], row['nodes'])} | {row['waiting_actors']} | {row['window_s']:.0f} | "
              f"{row['durable_transactions_per_s']:.2f} | {labels} | {statements} | {wal} | {connections} | {stored} |")


def store_table(rows):
    print("| Operation | Nodes | Ops/s | Actors/s | Empty claims | p50 ms | p99 ms | S2 sketch p50 / p99 ms | p99 change vs S2 |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|---:|")
    for row in (r for r in rows if r["kind"] == "store"):
        lat = row.get("latency") or row.get("end_to_end")
        s2 = S2.get((row["operation"], row["nodes"]))
        change = f"{(lat['p99_ms'] / s2[1] - 1) * 100:+.0f}%" if s2 else "-"
        print(f"| {row['operation']} | {row['nodes']} | {row.get('operations_per_s', 0):.0f} | {row.get('actors_per_s', 0):.0f} | "
              f"{row.get('empty_fraction', 0) * 100:.0f}% | {lat['p50_ms']:.3f} | {lat['p99_ms']:.3f} | "
              f"{f'{s2[0]:.3f} / {s2[1]:.3f}' if s2 else '-'} | {change} |")
    for row in (r for r in rows if r["kind"] == "store" and r["operation"].startswith("wake")):
        print(f"\nWake at {row['nodes']} owner node(s), {row['deliveries']} deliveries: mail commit p50/p99 "
              f"{row['commit']['p50_ms']:.3f}/{row['commit']['p99_ms']:.3f} ms; publish to delivery p50/p99 "
              f"{row['publish_to_delivery']['p50_ms']:.3f}/{row['publish_to_delivery']['p99_ms']:.3f} ms.")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inputs", nargs="+", required=True)
    args = parser.parse_args()
    rows = load(args.inputs)
    for title, table in [("Tool rounds", rounds_table), ("Writes per turn", writes_table),
                         ("Transactions by label", labels_table), ("Concurrency", concurrent_table),
                         ("Cold resume after N rounds", lambda r: resume_table(r, "resume-")),
                         ("Cold resume after prior turns (H2)", lambda r: resume_table(r, "prior-")),
                         ("Processes", parked_table), ("Cell snapshots (H5)", cell_table),
                         ("Idle (H8)", idle_table), ("Store (S2 re-run)", store_table)]:
        print(f"\n### {title}\n")
        table(rows)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
