#!/usr/bin/env python3
"""Produce process heap profiles and one matched on/off overhead observation.

Functional diagnostics only. Shared-host elapsed differences are observations,
not profiler cost estimates, timing baselines or growth/regression certificates.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import time
from pathlib import Path

from perf_artifacts import artifacts


def feature_binary(root: Path, package: str, binary: str, features: set[str]) -> str:
    prefix = f"//crates/{package}:{binary}__bin__fv_"
    inventory = json.loads((root / "tools/buck2/target-inventory.json").read_text())
    labels = {row["label"] for row in inventory["feature_lane_units"]
              if row["kind"] == "bin" and row["label"].startswith(prefix)
              and set(row["features"]) == features}
    if len(labels) != 1:
        raise ValueError(f"expected one {prefix} target for {features}, found {labels}")
    return labels.pop()


def profile_heaptrack(args: argparse.Namespace) -> None:
    """Track one persistent node; reuse its existing, flushed /proc wave series."""
    root = Path(__file__).resolve().parents[1]
    out = args.out_dir.resolve()
    out.mkdir(parents=True, exist_ok=False)
    for tool in ("heaptrack", "heaptrack_print", "zstd", "readelf"):
        if not shutil.which(tool):
            raise RuntimeError(f"--heaptrack requires {tool} on PATH")
    interpreter = (Path(shutil.which("heaptrack")).resolve().parent.parent /
                   "lib/heaptrack/libexec/heaptrack_interpret")
    if not interpreter.is_file():
        raise RuntimeError(f"heaptrack interpreter missing: {interpreter}")
    label = "//crates/lash-perf:lash-perf__bin"
    binary = artifacts(root, [label], build=not args.no_build,
                       report=args.build_report or out / "build.json",
                       optimized=True, symbolized=True)[label]
    linkage = subprocess.check_output(["readelf", "-W", "-l", "-S", "--dyn-syms", str(binary)], text=True)
    (out / "linkage.txt").write_text(linkage)
    if not all(marker in linkage for marker in ("INTERP", ".debug_line", ".symtab")):
        raise RuntimeError("heaptrack requires a dynamically linked, symbolized profiling binary")
    if not re.search(r"UND\s+malloc(?:@|\s)", linkage):
        raise RuntimeError("allocator does not import malloc; heaptrack interception is unproved")

    workload_path = out / "boundary.json"
    waves_path = workload_path.with_suffix(".waves.jsonl")
    command = ["heaptrack", "--raw", "--record-only", "--output", str(out / "heaptrack"),
               str(binary), "boundary", "--case", "persistent-node-waves",
               "--operations", str(args.operations), "--callers", str(args.callers),
               "--out", str(workload_path), "--store-dir", str(out / "store")]
    # The first flushed workload sample supplies the actual profiled PID.
    # Observe its preload, and bound its clock origin without another RSS sampler.
    origin_bounds = None
    preload = []
    start = time.monotonic_ns()
    previous = start
    env = dict(os.environ, TOKIO_WORKER_THREADS=str(args.worker_threads))
    with (out / "run.log").open("w") as log:
        with subprocess.Popen(command, cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT) as process:
            while process.poll() is None:
                if origin_bounds is None and waves_path.exists():
                    first = waves_path.read_text().splitlines()
                    if first:
                        sample = json.loads(first[0])
                        now = time.monotonic_ns()
                        origin_bounds = [previous - start - sample["elapsed_ns"],
                                         now - start - sample["elapsed_ns"]]
                        try:
                            maps = Path(f"/proc/{sample['process_id']}/maps").read_text()
                            preload = sorted({line.split()[-1] for line in maps.splitlines()
                                              if "libheaptrack_preload" in line})
                        except OSError:
                            pass  # Short populations still require allocation-site proof below.
                previous = time.monotonic_ns()
                time.sleep(0.1)
        exit_code = process.returncode
    elapsed = time.monotonic_ns() - start
    if exit_code != 0:
        raise RuntimeError(f"heaptrack population failed ({exit_code}); see {out / 'run.log'}")
    raw = out / "heaptrack.raw.zst"
    if not raw.exists() or raw.stat().st_size == 0:
        raise RuntimeError("heaptrack produced no .zst recording; see run.log")
    # Resolve symbols after exit so interpretation cannot back-pressure the
    # measured workload through heaptrack's pipe.
    recording = out / "heaptrack.zst"
    with recording.open("wb") as stream, (out / "interpret.log").open("w") as log:
        with subprocess.Popen(["zstd", "-dc", str(raw)], stdout=subprocess.PIPE, stderr=log) as decoder:
            with subprocess.Popen([str(interpreter)], stdin=decoder.stdout,
                                  stdout=subprocess.PIPE, stderr=log) as symbols:
                decoder.stdout.close()
                compressor = subprocess.Popen(["zstd", "-c"], stdin=symbols.stdout,
                                              stdout=stream, stderr=log)
                symbols.stdout.close()
                compressed = compressor.wait()
            interpreted = symbols.returncode
        decoded = decoder.returncode
    if any(code != 0 for code in (decoded, interpreted, compressed)):
        raise RuntimeError("heaptrack interpretation failed; see interpret.log")
    workload = json.loads(workload_path.read_text())
    if workload["allocations"]["mode"] != "stats_alloc":
        raise RuntimeError("expected StatsAlloc<System>; refusing an unproved allocator")
    if workload["population"] != args.operations * args.callers:
        raise RuntimeError("persistent-node population did not complete")
    # Parse the long recording once for all three rankings and the time series.
    print_command = ["heaptrack_print", "--file", str(recording),
                     "--merge-backtraces=0", "--shorten-templates=0", "--peak-limit=10",
                     "--print-peaks=1", "--print-leaks=1", "--print-allocators=1",
                     "--print-temporary=0", "--disable-builtin-suppressions",
                     "--disable-embedded-suppressions", "--print-massif",
                     str(out / "heap-over-time.massif"), "--massif-detailed-freq=10"]
    with (out / "top-sites.txt").open("w") as text, (out / "print.log").open("w") as log:
        subprocess.run(print_command, cwd=root, stdout=text, stderr=log, check=True)
    combined = (out / "top-sites.txt").read_text()
    headings = {"allocations": "MOST CALLS TO ALLOCATION FUNCTIONS\n",
                "peak": "PEAK MEMORY CONSUMERS\n", "leaked": "MEMORY LEAKS\n"}
    positions = {name: combined.index(heading) for name, heading in headings.items()}
    statistics = combined.index("total runtime:")
    summaries = {}
    for name, start_index in positions.items():
        stop = min(index for index in [*positions.values(), statistics] if index > start_index)
        summaries[name] = combined[start_index:stop] + combined[statistics:]
        (out / f"top-{name}.txt").write_text(summaries[name])
    # Empty/native-only profiles can succeed even when a Rust allocator bypasses
    # malloc. Require observed Rust workload stacks, not just a loaded library.
    counts = re.search(r"calls to allocation functions:\s*([\d,]+)", summaries["allocations"])
    rust_symbols = sorted({symbol for summary in summaries.values() for symbol in
                           re.findall(r"[^\n]*\blash_(?:perf|core|sqlite_store)::[^\n]*", summary)})
    if not counts or int(counts[1].replace(",", "")) == 0 or not rust_symbols:
        raise RuntimeError("allocator interception or Rust symbols unproved; inspect top-*.txt")
    allocation_calls = int(counts[1].replace(",", ""))
    if allocation_calls < workload["allocations"]["allocations"]:
        raise RuntimeError("heaptrack saw fewer calls than the Rust allocator; interception is incomplete")
    history = out / "heap-over-time.massif"
    if not history.exists() or "heap_tree=detailed" not in history.read_text():
        raise RuntimeError("heaptrack produced no detailed retained-site history")
    waves = [json.loads(line) for line in waves_path.read_text().splitlines()]
    if len(waves) != args.operations + 2 or any(row["rss_kib"] is None for row in waves):
        raise RuntimeError("missing existing /proc RSS samples")
    if len({row["process_id"] for row in waves}) != 1:
        raise RuntimeError("RSS samples do not describe one profiled process")
    rss_path = out / "rss.jsonl"
    with rss_path.open("w") as stream:
        for row in waves:
            stream.write(json.dumps({key: row[key] for key in
                                    ("process_id", "wave_index", "completed_turns", "node_alive",
                                     "elapsed_ns", "rss_kib", "rss_high_water_kib")}) + "\n")
    raw_bytes = raw.stat().st_size
    # Successful interpretation retains the complete allocation history. Its
    # much larger raw input and the fresh workload store are scratch, not evidence.
    raw.unlink()
    shutil.rmtree(out / "store")
    receipt = {
        "kind": "lash.heaptrack", "certifying": False, "growth_gate": None,
        "command": command, "exit_code": exit_code, "label": label,
        "tokio_worker_threads": args.worker_threads,
        "platform": "//tools/buck2:profiling", "executable": str(binary),
        "executable_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "heaptrack_version": subprocess.check_output(["heaptrack", "--version"], text=True).strip(),
        "allocator": "StatsAlloc<System>", "malloc_import": True,
        "observed_preload_libraries": preload, "resolved_workload_symbols": rust_symbols,
        "allocation_calls": allocation_calls,
        "rust_allocator_calls": workload["allocations"]["allocations"],
        "process_id": waves[0]["process_id"], "scope": "parent_process_only",
        "children_captured": False,
        "child_capture_note": "No child capture is claimed. persistent-node-waves uses one "
                              "in-process durable node and synthetic provider, without VM workers.",
        "elapsed_ns": elapsed, "population_elapsed_ns": waves[-1]["elapsed_ns"],
        "rss_source": "perf_support::memory::process_memory_sample via boundary waves",
        "rss_clock": "nanoseconds since population start, same PID and run as heaptrack",
        "population_origin_after_wrapper_spawn_ns_bounds": origin_bounds,
        "heaptrack_clock": "milliseconds since heaptrack initialization; includes CLI/setup/shutdown",
        "clock_alignment": "population origin bounded by first flushed sample observation; "
                           "wrapper spawn precedes heaptrack initialization; clocks are not identical",
        "rss_samples": len(waves), "rss_start_kib": waves[0]["rss_kib"],
        "rss_end_kib": waves[-1]["rss_kib"],
        "rss_last_live_node_kib": waves[-2]["rss_kib"],
        "recording": str(recording), "rss_series": str(rss_path),
        "raw_recording_bytes": raw_bytes, "raw_recording_retained": False,
        "workload_store_retained": False, "symbol_interpretation": "after_workload_exit",
        "site_history": str(out / "heap-over-time.massif"),
        "top_sites": {name: str(out / f"top-{name}.txt") for name in summaries},
    }
    (out / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"Heaptrack receipt: {out / 'receipt.json'}")


def profile_pair(args: argparse.Namespace) -> None:
    root = Path(__file__).resolve().parents[1]
    out = args.out_dir.resolve()
    out.mkdir(parents=True, exist_ok=False)
    modes = ("off", "dhat")
    labels = {mode: feature_binary(root, "lash-perf", "lash-perf", features)
              for mode, features in zip(modes, (set(), {"dhat-heap"}))}
    paths = artifacts(root, list(labels.values()), build=not args.no_build,
                      report=args.build_report or out / "build.json",
                      optimized=True, symbolized=True)
    if args.population == "boundary":
        modes = ("off", "future-sizes", "dhat")
        labels["future-sizes"] = labels["off"]
    observations = []
    for mode in modes:
        directory = out / mode
        directory.mkdir()
        executable = paths[labels[mode]]
        if args.population == "latency":
            command = [str(executable), "latency", "--cases", args.case,
                       "--lanes", "1", "--scale-down", "--out", str(directory / "latency.json"),
                       "--store-dir", str(directory / "store")]
        else:
            command = [str(executable), "boundary", "--case", args.case,
                       "--operations", str(args.operations), "--callers", str(args.callers),
                       "--out", str(directory / "boundary.json"),
                       "--store-dir", str(directory / "store")]
        if mode == "dhat":
            command += ["--dhat-out", str(directory / "parent.dhat.json"), "--dhat-frames", "16"]
        if mode == "future-sizes":
            command += ["--future-out", str(directory / "future-sizes.json"), "--future-top", "20"]
        with (directory / "run.log").open("w") as log:
            start = time.monotonic_ns()
            result = subprocess.run(command, cwd=root, stdout=log, stderr=subprocess.STDOUT)
            elapsed = time.monotonic_ns() - start
        # A selected latency diagnostic deliberately has no fast gate population.
        # Check its case errors below; exit 2 is not accepted on its own.
        valid = result.returncode == 0
        if args.population == "latency" and result.returncode == 2:
            receipt = json.loads((directory / "latency.json").read_text())
            valid = bool(receipt["cases"]) and all(case["samples"] > 0 and not case.get("errors")
                                                  and case["non_answered"] == 0
                                                  for case in receipt["cases"])
        row = {"mode": mode, "command": command, "exit_code": result.returncode,
               "completed": valid, "elapsed_ns": elapsed,
               "elapsed_statistic": "single_process_spawn_through_exit_including_profile_flush",
               "process_scope": "population_parent_and_waited_children",
               "executable_sha256": hashlib.sha256(paths[labels[mode]].read_bytes()).hexdigest(),
               "parent_executable_sha256": hashlib.sha256(executable.read_bytes()).hexdigest(),
               "label": labels[mode]}
        observations.append(row)
        if not valid:
            raise RuntimeError(f"population failed; see {directory / 'run.log'}")
        if mode == "dhat":
            profiles = sorted(directory.rglob("*.dhat.json"))
            expected = 2 if args.population == "latency" else 1
            if len(profiles) != expected:
                raise RuntimeError(f"expected {expected} process profiles, found {profiles}")
            for path in profiles:
                profile = json.loads(path.read_text())
                if not profile.get("pps"):
                    raise RuntimeError(f"empty allocation profile: {path}")
                if not path.with_suffix(".receipt.json").exists():
                    raise RuntimeError(f"missing profile window receipt: {path}")
            row["profiles"] = [str(path) for path in profiles]
    baseline = observations[0]["elapsed_ns"]
    configuration = ({"operations": args.operations, "callers": args.callers}
                     if args.population == "boundary" else
                     {"lanes": 1, "scale_down": True})
    receipt = {"kind": "lash.heap-profile-overhead", "population": args.population,
               "case": args.case,
               "configuration": configuration,
               "statistic": "one_matched_population_pair_per_instrument",
               "quantity": "observed_population_wall_duration_difference", "unit": "nanoseconds",
               "window": "parent_spawn_through_exit_including_children_and_profile_flush",
               "process_id": "separate_processes_per_mode; see_process_profile_receipts",
               "order": list(modes), "certifying": False, "growth_gate": None,
               "off_mode": "stats_alloc",
               "build": "same_symbolized_optimized_platform; feature_lanes_differ_by_instrument",
               "observations": observations,
               "on_minus_off_ns": {row["mode"]: row["elapsed_ns"] - baseline
                                   for row in observations[1:]}}
    (out / "overhead.json").write_text(json.dumps(receipt, indent=2) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--population", choices=("boundary", "latency"), default="boundary")
    parser.add_argument("--case", default="root-redrive")
    parser.add_argument("--operations", type=int, default=1)
    parser.add_argument("--callers", type=int, default=1)
    parser.add_argument("--out-dir", type=Path, required=True, help="Fresh evidence directory")
    parser.add_argument("--build-report", type=Path)
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--heaptrack", action="store_true",
                        help="Track persistent-node-waves with System malloc and its existing RSS series")
    parser.add_argument("--worker-threads", type=int, default=2,
                        help="Tokio worker threads in heaptrack mode (default: 2)")
    args = parser.parse_args()
    if args.population == "latency" and args.case not in ("cross-worker", "poll", "grace"):
        parser.error("latency heap pairs select one child-node case: cross-worker, poll or grace")
    if args.operations < 1 or args.callers < 1 or args.worker_threads < 1:
        parser.error("operations, callers and worker threads must be positive")
    if args.heaptrack:
        if args.population != "boundary" or args.case != "persistent-node-waves":
            parser.error("--heaptrack requires --population boundary --case persistent-node-waves")
        profile_heaptrack(args)
    else:
        profile_pair(args)


if __name__ == "__main__":
    main()
