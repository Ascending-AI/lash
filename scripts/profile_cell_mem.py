#!/usr/bin/env python3
"""One cell's allocation sites, guest capacity ledger, RSS and stack watermark."""
from __future__ import annotations
import argparse
from collections import defaultdict
import hashlib
import json
from pathlib import Path
import resource
import re
import subprocess
import time
from perf_artifacts import artifacts

def cell_label(root):
    inventory = json.loads((root / "tools/buck2/target-inventory.json").read_text())
    labels = {unit["label"] for unit in inventory["feature_lane_units"]
              if unit["kind"] == "example" and ":cell_mem__example" in unit["label"]}
    if len(labels) != 1:
        raise RuntimeError(f"expected one cell memory example target, found {labels}")
    return labels.pop()


def cap():
    resource.setrlimit(resource.RLIMIT_AS, (8 << 30, 8 << 30))


def site_name(row, table):
    frames = [name for ip in row["frames"] for name in table.get(str(ip), [])]
    owning = [name for name in frames if "crates/lash-" in name
              and "cell_memory" not in name
              and not name.startswith(("__rust_", "__rg_", "__rdl_"))
              and re.search(r"\.rs:[1-9][0-9]*$", name)]
    return owning[0] if owning else next((s for s in frames if "alloc::" not in s
                                                     and "backtrace::" not in s), "unresolved")


def describe(profile, records):
    groups = defaultdict(lambda: defaultdict(int))
    resolved = total = reallocations = 0
    for row in profile["sites"]:
        name = site_name(row, profile["frame_table"])
        stats = row["stats"]
        for key, value in stats.items():
            if key != "peak_live":
                groups[name][key] += value
        # Independent per-context/site maxima can overlap. Do not add them.
        groups[name]["largest_site_peak"] = max(groups[name]["largest_site_peak"], stats["peak_live"])
        total += stats["allocations"]
        reallocations += stats["reallocations"]
        if name != "unresolved":
            resolved += stats["allocations"]
    print(f"\nPID {profile['process_id']}: {total:,} allocations plus {reallocations:,} reallocations; "
          f"{resolved / total:.2%} resolved by site" if total else "empty allocation profile")
    ordered = sorted(groups.items(), key=lambda item: item[1]["allocations"], reverse=True)
    covered = 0
    count = 0
    while count < len(ordered) and (count < 10 or covered < total * 0.95):
        covered += ordered[count][1]["allocations"]
        count += 1
    print(f"Largest {count} sites cover {covered / total:.2%} of allocations" if total else "No allocation sites")
    print("allocations\treallocations\talloc/record\tbytes\tlargest site peak\tlive at guest peak\tend live\tRust site")
    for name, stats in ordered[:count]:
        per_record = f"{stats['allocations'] / records:.4f}" if records else "n/a"
        print(f"{stats['allocations']}\t{stats['reallocations']}\t{per_record}\t{stats['bytes']}\t{stats['largest_site_peak']}\t"
              f"{stats['at_guest_peak']}\t{stats['live']}\t{name}")
    for quantity in ("bytes", "at_guest_peak", "live"):
        print(f"\nTop source sites by {quantity}, bytes\tcount\tRust site")
        for name, stats in sorted(groups.items(), key=lambda item: item[1][quantity], reverse=True)[:10]:
            print(f"{stats[quantity]}\t{stats['count']}\t{name}")
    print("\nGuest context joins, count / operations, denominator is native calls or code statements + expressions")
    for row in sorted(profile["sites"], key=lambda row: row["stats"]["count"], reverse=True)[:10]:
        print(f"{row['stats']['count']}\t{row['guest_operations']}\t"
              f"{row['allocations_per_guest_operation']}\t{row['guest']}")
    print("\nGuest charged capacity by kind\tpeak verdict\tend")
    for name, peak, end in zip(("resident values", "control", "shared layouts", "sealed floor excess"),
                               profile["guest_peak_ledger"], profile["end_ledger"]):
        print(f"{name}\t{peak}\t{end}")
    print(f"Window heap peak={profile['peak_live']} end={profile['end_live']} bytes")
    print("Cell-window RSS high-water reset: " + str(profile.get("rss_peak_reset", False)))
    if not profile.get("rss_peak_reset"):
        print("RSS high-water remains process-lifetime; do not attribute it to this cell.")
    print("Process\tbaseline\tat guest peak\tend")
    for key in ("rss_bytes", "rss_high_water_bytes", "requested_process_bytes", "glibc_in_use_bytes", "glibc_free_arena_bytes",
                "anonymous_rss_bytes", "file_rss_bytes", "stack_rss_bytes"):
        print(key + "\t" + "\t".join(str((profile[phase] or {}).get(key, "unavailable")) for phase in
                                        ("baseline", "guest_peak_process", "end_process")))
    print("RSS includes resident allocator slack, observer storage, file mappings and stacks. "
          "Requested bytes and allocator capacity can include nonresident pages.")
    peak = profile["guest_peak_process"] or {}
    peak_live = sum(row["stats"]["at_guest_peak"] for row in profile["sites"])
    if all(isinstance(peak.get(key), int) for key in
           ("rss_bytes", "requested_process_bytes", "glibc_in_use_bytes", "glibc_free_arena_bytes")):
        print(f"At guest peak: live heap={peak_live}, RSS minus live={peak['rss_bytes'] - peak_live}")
        print("preexisting/observer requested\t" + str(peak["requested_process_bytes"] - peak_live))
        print("glibc in-use minus requested\t" + str(peak["glibc_in_use_bytes"] - peak["requested_process_bytes"]))
        print("free allocator arenas\t" + str(peak["glibc_free_arena_bytes"]))
        print("RSS minus glibc capacity, mappings/residency remainder\t" +
              str(peak["rss_bytes"] - peak["glibc_in_use_bytes"] - peak["glibc_free_arena_bytes"]))
    end = profile["end_process"]
    live = profile["end_live"]
    requested = end.get("requested_process_bytes")
    in_use = end.get("glibc_in_use_bytes")
    free = end.get("glibc_free_arena_bytes")
    anon = end.get("anonymous_rss_bytes")
    if all(isinstance(value, int) for value in (requested, in_use, free, anon)):
        terms = {
            "preexisting_and_observer_requested_bytes": requested - live,
            "glibc_in_use_minus_requested_bytes": in_use - requested,
            "allocator_free_arena_bytes": free,
            "anonymous_residency_minus_allocator_capacity_bytes": anon - in_use - free,
            "file_mapping_rss_bytes": end["file_rss_bytes"],
            "native_stack_rss_bytes": end["stack_rss_bytes"],
            "other_mapping_rss_bytes": end["other_rss_bytes"],
        }
        gap = end["rss_bytes"] - live
        if sum(terms.values()) != gap:
            raise RuntimeError("RSS reconciliation arithmetic does not balance")
        print(f"RSS minus window live heap = {gap} bytes. Signed reconciliation terms:")
        for name, value in terms.items():
            print(f"{name}\t{value}")
        print("The anonymous residual combines nonallocator mappings and nonresident allocator "
              "capacity; no individual cause is inferred from its sign.")
    print("Native stack: " + json.dumps(profile["stack"], sort_keys=True))
    return {"allocation_count": total, "resolved_allocation_count": resolved,
            "allocations_per_record": total / records if records else None, "reallocations": reallocations,
            "printed_sites": count, "printed_allocation_coverage": covered / total if total else None,
            "sites": dict(groups)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("file", type=Path)
    parser.add_argument("--worker", action="store_true")
    parser.add_argument("--stack", action="store_true")
    parser.add_argument("--cells", type=int, default=1, help="worker cells in one process, reset between cells")
    parser.add_argument("--records", type=int, help="explicit useful-record denominator")
    parser.add_argument("--out-dir", type=Path)
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--build-report", type=Path)
    args = parser.parse_args()
    if not args.file.is_file() or args.file.suffix not in (".ts", ".py"):
        parser.error("file must be an existing .ts or .py cell")
    if args.cells <= 0 or (not args.worker and args.cells != 1):
        parser.error("--cells must be positive and requires --worker")
    if args.records is not None and args.records <= 0:
        parser.error("--records must be positive")
    root = Path(__file__).resolve().parents[1]
    out = (args.out_dir or root / ".kiln/cell-mem" / str(time.time_ns())).resolve()
    out.mkdir(parents=True, exist_ok=False)
    label = cell_label(root)
    binary = artifacts(root, [label], build=not args.no_build,
                       report=args.build_report or out / "build.json",
                       optimized=True, symbolized=True)[label]
    receipts = {}
    for mode in ("off", "on"):
        directory = out / mode
        directory.mkdir()
        command = [str(binary), str(args.file.resolve()), str(directory)]
        if mode == "off":
            command.append("--off")
        if args.worker:
            command.extend(["--worker", "--cells", str(args.cells)])
        if args.stack:
            command.append("--stack")
        counter = directory / "instructions.csv"
        with (directory / "run.log").open("w") as log:
            subprocess.run(["perf", "stat", "-x", ";", "-e", "instructions:u", "-o", str(counter),
                            "--", *command], cwd=root, stdout=log, stderr=subprocess.STDOUT,
                           preexec_fn=cap, check=True)
        rows = [json.loads(line) for line in (directory / "run.log").read_text().splitlines()
                if line.startswith('{"')]
        if not rows:
            raise RuntimeError(f"no completed cell receipts: {directory}")
        fields = next(line.split(";") for line in counter.read_text().splitlines()
                      if "instructions:u" in line)
        receipts[mode] = {"cells": rows, "instructions": int(fields[0]), "perf_counter_fields": fields}
    if [r["result"] for r in receipts["off"]["cells"]] != [r["result"] for r in receipts["on"]["cells"]]:
        raise RuntimeError("observed cell results differ")
    if [r["charged"] for r in receipts["off"]["cells"]] != [r["charged"] for r in receipts["on"]["cells"]]:
        raise RuntimeError("observed cell charge differs")
    profiles = sorted((out / "on").glob("vm-worker-*-cell-*.json")) if args.worker else [out / "on/cell-memory.json"]
    if len(profiles) != (args.cells if args.worker else 1):
        raise RuntimeError("missing per-cell allocation windows")
    if args.worker:
        for mode in ("off", "on"):
            receipts[mode]["cell_requests"] = [json.loads(path.read_text())["requests"]
                for path in sorted((out / mode).glob("vm-worker-*-cell-*.json"))]
    else:
        for mode in ("off", "on"):
            receipts[mode]["cell_requests"] = [row["requests"] for row in receipts[mode]["cells"]]
    print("Allocator requests, observer off/on: " + str(receipts["off"]["cell_requests"]) + " / " + str(receipts["on"]["cell_requests"]))
    summaries = [describe(json.loads(path.read_text()), args.records) for path in profiles]
    overhead = {"executable_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                "source_sha256": hashlib.sha256(args.file.read_bytes()).hexdigest(),
                "modes": receipts, "instruction_ratio_on_over_off": receipts["on"]["instructions"] / receipts["off"]["instructions"],
                "scope":"whole process including startup, waited workers, observer bookkeeping, symbolization and output",
                "production_cost":False, "wall_time_claim":None}
    (out / "overhead.json").write_text(json.dumps(overhead, indent=2) + "\n")
    (out / "summary.json").write_text(json.dumps(summaries, indent=2) + "\n")
    print(f"\nSame-executable observer off/on instructions:u: {receipts['off']['instructions']:,} / "
          f"{receipts['on']['instructions']:,}, ratio {overhead['instruction_ratio_on_over_off']:.3f}. "
          "Diagnostic observer cost; shared-host wall time unclaimed.")
    print(f"Artifacts: {out}")


if __name__ == "__main__":
    main()
