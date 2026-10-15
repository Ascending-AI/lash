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
import resource
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


def profile_growturn(args: argparse.Namespace) -> None:
    """Profile both host lifetimes of the growing turn, with explicit workers."""
    root = Path(__file__).resolve().parents[1]
    out = args.out_dir.resolve()
    out.mkdir(parents=True, exist_ok=False)
    host = feature_binary(root, "lash-perf", "growturn", {"dhat-heap"})
    worker = feature_binary(root, "lash-vm-worker", "lash-vm-worker",
                            {"dhat-heap", "dialect-python", "dialect-typescript", "testing"})
    paths = artifacts(root, [host, worker], build=not args.no_build,
                      report=args.build_report or out / "build.json",
                      optimized=True, symbolized=True)
    observations = []
    for phase in ("start", "resume"):
        command = [str(paths[host]), "--phase", phase, "--rounds", str(args.operations),
                   "--payload-bytes", str(args.payload_bytes),
                   "--checkpoint-every", str(args.checkpoint_every),
                   "--database", str(out / "turn.sqlite"), "--vm-worker", str(paths[worker]),
                   "--out", str(out / "checkpoints.jsonl"),
                   "--profile-from-round", str(args.profile_from_round),
                   "--worker-profile-dir", str(out / "workers")]
        if not args.profile_at_close or phase == "resume":
            command += ["--dhat-out", str(out / f"{phase}.dhat.json"), "--dhat-frames", "16"]
        if args.profile_at_close:
            command += ["--profile-at-close"]
        with (out / f"{phase}.log").open("w") as log:
            result = subprocess.run(command, cwd=root, stdout=log, stderr=subprocess.STDOUT)
        observations.append({"phase": phase, "command": command, "exit_code": result.returncode})
        if result.returncode:
            raise RuntimeError(f"growing turn {phase} failed; see {out / (phase + '.log')}")
    rows = [json.loads(line) for line in (out / "checkpoints.jsonl").read_text().splitlines()]
    completed = [row for row in rows if row.get("phase") == "completed"]
    closed = [row for row in rows if row.get("phase") == "closed"]
    if (len(completed) != 1 or completed[0]["rounds_done"] != args.operations
            or completed[0]["store"]["completed"] != 1
            or completed[0]["store"]["errors"] or completed[0]["store"]["unfinished"]
            or len(closed) != 1 or not closed[0]["close_tombstone_observed"]):
        raise RuntimeError("growing turn completion/close receipt is incomplete")
    profiles = sorted(out.rglob("*.dhat.json"))
    if not any(path.parent == out for path in profiles):
        raise RuntimeError("no host profiling window reached the configured round")
    if not any(path.parent.name == "workers" for path in profiles):
        raise RuntimeError("no worker profile was flushed at clean reset")
    for path in profiles:
        if not json.loads(path.read_text()).get("pps") or not path.with_suffix(".receipt.json").exists():
            raise RuntimeError(f"empty profile or missing scope receipt: {path}")
    receipt = {"kind": "lash.growturn-heap", "certifying": False, "growth_gate": None,
               "rounds": args.operations, "payload_bytes": args.payload_bytes,
               "allocator": "dhat-heap+stats_alloc", "observations": observations,
               "profile_from_round": args.profile_from_round,
               "profile_at_close": args.profile_at_close,
               "host_scope": ("resumed process setup through durable close; core still alive" if args.profile_at_close
                              else "full process lifetime") if not args.profile_from_round else
                             "from selected completed round; earlier allocations excluded; end at close if requested",
               "completed": 1, "errors": 0, "unfinished": 0,
               "profiles": [str(path) for path in profiles],
               "worker_scope": "per-process bootstrap through first clean reset; not every later cell",
               "executables": {label: {"path": str(path),
                   "sha256": hashlib.sha256(path.read_bytes()).hexdigest()} for label, path in paths.items()}}
    (out / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"Growing-turn heap receipt: {out / 'receipt.json'}")


def profile_replay(args: argparse.Namespace) -> None:
    """Use one optimized DHAT executable for an inactive/selected-turn pair."""
    root = Path(__file__).resolve().parents[1]
    out = args.out_dir.resolve()
    out.mkdir(parents=True, exist_ok=False)
    label = feature_binary(root, "lash-perf", "lash-perf", {"dhat-heap"})
    worker = "//crates/lash-vm-worker:lash-vm-worker__bin"
    build_report = args.build_report or out / "build.json"
    paths = artifacts(root, [label, worker], build=not args.no_build,
                      report=build_report, optimized=True, symbolized=True)
    observations = []
    last = args.profile_turn if args.profile_through_turn is None else args.profile_through_turn

    def cap_memory() -> None:
        limit = 8 * 1024**3
        soft, hard = resource.getrlimit(resource.RLIMIT_AS)
        resource.setrlimit(resource.RLIMIT_AS, (min(soft, limit) if soft >= 0 else limit, hard))

    for mode in ("off", "dhat"):
        directory = out / mode
        directory.mkdir()
        store = root / "target/heap-replay" / f"{os.getpid()}-{mode}"
        command = [str(paths[label]), "replay", args.workload, "--turns", str(args.turns),
                   "--store-dir", str(store), "--out", str(directory / "replay.json")]
        if mode == "dhat":
            command += ["--dhat-out", str(directory / "parent.dhat.json"),
                        "--dhat-frames", "64", "--profile-turn", str(args.profile_turn),
                        "--profile-through-turn", str(last)]
        with (directory / "run.log").open("w") as log:
            started = time.monotonic_ns()
            result = subprocess.run(["/usr/bin/time", "-v", "-o", str(directory / "resources.txt"),
                                     *command], cwd=root, stdout=log, stderr=subprocess.STDOUT,
                                    env=dict(os.environ, LASH_VM_WORKER=str(paths[worker])),
                                    preexec_fn=cap_memory)
            elapsed = time.monotonic_ns() - started
        if result.returncode:
            raise RuntimeError(f"replay failed; see {directory / 'run.log'}")
        replay = json.loads((directory / "replay.json").read_text())
        identity = replay["identity"]
        flags = identity["build_profile"]
        if (identity["allocator"] != "dhat-heap+stats_alloc" or "OPT_LEVEL=3;" not in flags
                or "debug_assertions=false" not in flags or "debuginfo=line-tables-only" not in flags
                or "force-frame-pointers=yes" not in flags):
            raise RuntimeError("replay heap capture requires the symbolized optimized DHAT build")
        workload = replay["workloads"][0]
        if len(workload["rounds"]) < last:
            raise RuntimeError("replay did not reach the selected turn range")
        selected = [row for row in workload["rounds"] if args.profile_turn <= row["turn"] <= last]
        observations.append({"mode": mode, "command": command, "exit_code": result.returncode,
                             "elapsed_ns": elapsed, "identity": replay["identity"],
                             "script_digest": workload["script_digest"],
                             "completed_turns": len(workload["rounds"]),
                             "selected_turns": [row["turn"] for row in selected],
                             "selected_turn_wall_ms": sum(row["metrics"]["wall_ms"] for row in selected),
                             "resources": str(directory / "resources.txt")})
        shutil.rmtree(store)
    profile = out / "dhat/parent.dhat.json"
    window = json.loads(profile.with_suffix(".receipt.json").read_text())
    if (not window["completed"] or window["first_turn"] != args.profile_turn
            or window["last_turn"] != last
            or not json.loads(profile.read_text()).get("pps")):
        raise RuntimeError("selected replay window did not produce a complete populated profile")
    if observations[0]["script_digest"] != observations[1]["script_digest"]:
        raise RuntimeError("on/off workload identities differ")
    receipt = {"kind": "lash.replay-heap", "certifying": False,
               "population": "replay", "workload": args.workload, "turns": args.turns,
               "profile_turn": args.profile_turn,
               "profile_through_turn": last,
               "scope": "host Rust allocations in selected replay rounds; excludes preexisting history, worker heaps and native SQLite allocations",
               "window_receipt": window, "profile": str(profile), "build_report": str(build_report),
               "platform": "//tools/buck2:profiling", "allocator": "dhat-heap+stats_alloc",
               "statistic": "single ordered same-executable profiler inactive/active pair; shared-host diagnostic",
               "executables": {key: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                               for key, path in paths.items()},
               "observations": observations,
               "on_minus_off_ns": observations[1]["elapsed_ns"] - observations[0]["elapsed_ns"]}
    (out / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"Replay heap receipt: {out / 'receipt.json'}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--population", choices=("boundary", "latency", "growturn", "replay"), default="boundary")
    parser.add_argument("--workload", help="Replay workload name or script path")
    parser.add_argument("--turns", type=int, help="Replay prefix length")
    parser.add_argument("--profile-turn", type=int, help="Replay: one-based turn to profile")
    parser.add_argument("--profile-through-turn", type=int, help="Replay: inclusive last turn")
    parser.add_argument("--case", default="root-redrive")
    parser.add_argument("--operations", type=int, default=1)
    parser.add_argument("--payload-bytes", type=int, default=1024)
    parser.add_argument("--profile-from-round", type=int, default=0)
    parser.add_argument("--profile-at-close", action="store_true",
                        help="Growturn: profile resumed setup through close, before core shutdown")
    parser.add_argument("--checkpoint-every", type=int, default=100)
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
    if args.population == "replay":
        if (args.heaptrack or not args.workload or not args.turns or args.turns < 1
                or args.profile_turn is None
                or not 1 <= args.profile_turn <= ((args.profile_turn if args.profile_through_turn is None else args.profile_through_turn)) <= args.turns):
            parser.error("replay needs --workload, positive --turns and a profile turn range within turns")
        profile_replay(args)
    elif args.population == "growturn":
        if (args.heaptrack or args.operations < 4 or args.payload_bytes < 32
                or args.checkpoint_every < 1 or not 0 <= args.profile_from_round <= args.operations):
            parser.error("growturn uses dhat, rounds >= 4, payload >= 32 and positive checkpoints")
        profile_growturn(args)
    elif args.heaptrack:
        if args.population != "boundary" or args.case != "persistent-node-waves":
            parser.error("--heaptrack requires --population boundary --case persistent-node-waves")
        profile_heaptrack(args)
    else:
        profile_pair(args)


if __name__ == "__main__":
    main()
