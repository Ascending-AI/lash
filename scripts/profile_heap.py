#!/usr/bin/env python3
"""Produce process heap profiles and one matched on/off overhead observation.

Functional diagnostics only. Shared-host elapsed differences are observations,
not profiler cost estimates, timing baselines or growth/regression certificates.
"""
from __future__ import annotations

import argparse
import hashlib
import json
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


def profile_pair(args: argparse.Namespace) -> None:
    root = Path(__file__).resolve().parents[1]
    out = args.out_dir.resolve()
    out.mkdir(parents=True, exist_ok=False)
    modes = ("off", "dhat")
    paths = {}
    labels = {}
    if args.population == "vm-worker":
        matrix = feature_binary(root, "lash-perf", "vm-worker-matrix", set())
        workers = [feature_binary(root, "lash-vm-worker", "lash-vm-worker", features)
                   for features in ({"testing"}, {"testing", "dhat-heap"})]
        labels = {"off": workers[0], "dhat": workers[1]}
        paths = artifacts(root, [matrix, *workers], build=not args.no_build,
                          report=args.build_report or out / "build.json",
                          optimized=True, symbolized=True)
    else:
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
        executable = paths[matrix] if args.population == "vm-worker" else paths[labels[mode]]
        if args.population == "vm-worker":
            command = [str(executable), "--heap-smoke", "--worker", str(paths[labels[mode]]),
                       "--out", str(directory)]
            if mode == "dhat":
                command += ["--heap-profile-dir", str(directory / "profiles")]
        elif args.population == "latency":
            command = [str(executable), "latency", "--cases", args.case,
                       "--lanes", "1", "--scale-down", "--out", str(directory / "latency.json"),
                       "--store-dir", str(directory / "store")]
        else:
            command = [str(executable), "boundary", "--case", args.case,
                       "--operations", str(args.operations), "--callers", str(args.callers),
                       "--out", str(directory / "boundary.json"),
                       "--store-dir", str(directory / "store")]
        if mode == "dhat" and args.population != "vm-worker":
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
                     {"lanes": 1, "scale_down": True}
                     if args.population == "latency" else {"max_workers": 1})
    receipt = {"kind": "lash.heap-profile-overhead", "population": args.population,
               "case": "heap-smoke" if args.population == "vm-worker" else args.case,
               "configuration": configuration,
               "statistic": "one_matched_population_pair_per_instrument",
               "quantity": "observed_population_wall_duration_difference", "unit": "nanoseconds",
               "window": "parent_spawn_through_exit_including_children_and_profile_flush",
               "process_id": "separate_processes_per_mode; see_process_profile_receipts",
               "order": list(modes), "certifying": False, "growth_gate": None,
               "off_mode": "stats_alloc" if args.population != "vm-worker" else "System",
               "build": "same_symbolized_optimized_platform; feature_lanes_differ_by_instrument",
               "observations": observations,
               "on_minus_off_ns": {row["mode"]: row["elapsed_ns"] - baseline
                                   for row in observations[1:]}}
    (out / "overhead.json").write_text(json.dumps(receipt, indent=2) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--population", choices=("boundary", "latency", "vm-worker"), default="boundary")
    parser.add_argument("--case", default="root-redrive")
    parser.add_argument("--operations", type=int, default=1)
    parser.add_argument("--callers", type=int, default=1)
    parser.add_argument("--out-dir", type=Path, required=True, help="Fresh evidence directory")
    parser.add_argument("--build-report", type=Path)
    parser.add_argument("--no-build", action="store_true")
    args = parser.parse_args()
    if args.population == "latency" and args.case not in ("cross-worker", "poll", "grace"):
        parser.error("latency heap pairs select one child-node case: cross-worker, poll or grace")
    if args.operations < 1 or args.callers < 1:
        parser.error("operations and callers must be positive")
    profile_pair(args)


if __name__ == "__main__":
    main()
