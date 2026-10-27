#!/usr/bin/env python3
"""Local counted performance checks. No shared-host wall duration decides a verdict."""
from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import re
import statistics
import shutil
import subprocess
import tempfile
import uuid

ROOT = Path(__file__).resolve().parents[1]
IDENTITY = ("host", "compiler", "allocator", "build", "build_profile")


def execute(command: list[str], log: Path, env: dict[str, str]) -> None:
    with log.open("w") as output:
        result = subprocess.run(command, cwd=ROOT, env=env, stdout=output, stderr=subprocess.STDOUT, timeout=1200)
    if result.returncode:
        raise ValueError(f"producer failed ({result.returncode}); see {log}")


def read(path: Path) -> dict:
    return json.loads(path.read_text())


def instructions(binary: str, directory: Path, env: dict[str, str]) -> dict:
    small = "empty_loop,prop_read,prop_write,array_read,func_call,func_closure_call,int_arith,float_arith,map_set_string,string_build1,int_to_string,sort_bench,json_transform,records,templating"
    captures = []
    for group, workloads, factor in (("ordinary", small, "100"), ("large-objects", "object_build,object_sort", "10")):
        compare = directory / f"compare-{group}.json"
        execute([binary, "compare", "--out", str(compare), "--instructions", "--workload", workloads,
                 "--runs", "5", "--warmups", "1", "--factor", factor], directory / f"compare-{group}.log", env)
        capture = read(compare)
        if capture.get("errors") or not capture.get("rows"):
            raise ValueError(f"incomplete kernel comparison: {group}")
        captures.append(capture)
    kernel = captures[0]
    if any(c["cpu"] != kernel["cpu"] for c in captures):
        raise ValueError("kernel CPU affinity changed between populations")
    kernel["rows"] = [row for capture in captures for row in capture["rows"]]
    rows = {}
    geometry = {}
    provenance = {k: kernel[k] for k in ("commit", "recorded_at", "run_id")}
    artifacts = {}
    counts = {}
    experiments = {}
    for row in kernel["rows"]:
        if row["engine"] != "lash":
            raise ValueError("instruction gate accepts only kernel rows")
        name = f"kernel/{row['dialect']}/{row['workload']}"
        for pair in row["samples"]:
            for sample in pair:
                pmu = sample.get("instruction_window", {})
                enabled, running = pmu.get("time_enabled", 0), pmu.get("time_running", 0)
                if not enabled or not 0.99 <= running / enabled <= 1 or pmu.get("instructions", 0) <= 0:
                    raise ValueError(f"{name}: unqualified PMU interval")
        value = row.get("instructions", {}).get("median_per_operation", 0)
        if value <= 0:
            raise ValueError(f"{name}: missing positive instruction slope")
        rows[name] = value
        geometry[name] = {k: row[k] for k in ("source_hash", "trips", "operations")}
        geometry[name].update({k: kernel["geometry"][k] for k in ("runs", "warmups")})
        artifacts["kernel"] = row["engine_identity"]
    expected_kernel = {f"kernel/{dialect}/{name}" for dialect in ("typescript", "python")
        for name in ("empty_loop", "prop_read", "prop_write", "array_read", "func_call", "func_closure_call",
            "int_arith", "float_arith", "map_set_string", "string_build1", "int_to_string", "sort_bench",
            "json_transform", "records", "templating", "object_build", "object_sort")}
    if set(rows) != expected_kernel:
        raise ValueError("kernel comparison did not execute the complete row population")
    # Match the counted replay subset and lengths. Each producer is a separate
    # process so its host PMU opens before any runtime thread exists.
    policy = read(ROOT / "crates/lash-perf/workloads/replay/budgets.json")
    identity = None
    # Canonical SQLite paths enter publication identities and bound-byte counts.
    with tempfile.TemporaryDirectory(prefix="lash-instructions-", dir="/tmp") as store_dir:
        for spec in policy["subset"]:
            name = spec["workload"]
            out = directory / f"{name}.json"
            command = [binary, "replay", name, "--out", str(out), "--store-dir", store_dir, "--instructions"]
            if "turns" in spec:
                command += ["--turns", str(spec["turns"])]
            execute(command, directory / f"{name}.log", env)
            receipt = read(out)
            current_identity = {k: receipt["identity"][k] for k in IDENTITY}
            if identity is not None and current_identity != identity:
                raise ValueError("replay producer identity changed within population")
            identity = current_identity
            artifacts[name] = receipt["identity"]["executable_sha256"]
            workload, = receipt["workloads"]
            experiments[name] = workload["experiment"]
            for metric in ("sql_statements", "sql_writes", "allocations", "allocated_bytes", "worker_allocations", "worker_allocated_bytes"):
                if metric in workload["summary"]:
                    counts[f"replay/{name}/{metric}"] = {"median": workload["summary"][metric]["per_turn_median"],
                        "unit": workload["summary"][metric]["unit"]}
            geometry[f"replay/{name}"] = {k: workload[k] for k in ("script_digest", "backend", "turns", "sessions")}
            # CPU per operation is the median over steady rounds, each round's
            # host and worker counts retained separately. Setup is excluded.
            steady = [r for r in workload["rounds"] if not r.get("setup", False)]
            for process in ("host", "worker"):
                metric = f"{process}_instructions"
                if process == "worker" and workload["protocol"] == "standard":
                    continue
                values = []
                for row in steady:
                    m = row["metrics"]
                    enabled = m.get(f"{process}_instruction_enabled_ns", 0)
                    running = m.get(f"{process}_instruction_running_ns", 0)
                    if not enabled or not 0.99 <= running / enabled <= 1 or m.get(metric, 0) <= 0 or m.get("worker_instruction_refusals", 0):
                        raise ValueError(f"replay/{name}/{process}: unqualified PMU interval")
                    values.append(m[metric] / row["sessions"])
                if not values:
                    raise ValueError(f"replay/{name}/{process}: empty population")
                rows[f"replay/{name}/{process}"] = statistics.median(values)
    return {"identity": identity, "cpu": kernel["cpu"], "geometry": geometry, "rows": rows, "provenance": provenance, "artifacts": artifacts, "counted_metrics": counts, "experiments": experiments}


def startup(binary: str, directory: Path, env: dict[str, str]) -> dict:
    out, trace = directory / "startup.json", directory / "startup.strace"
    execute(["strace", "-f", "-qq", "-s", "256", "-e", "trace=clock_nanosleep,nanosleep", "-o", str(trace), binary, "vm-startup-gate", "--out", str(out)], directory / "startup.log", env)
    receipt = read(out)
    starts = receipt["starts"]
    if len(starts) != 20 or len({r["pid"] for r in starts}) != 20:
        raise ValueError("startup requires 20 distinct successful spawns")
    requested = {r["pid"]: 0 for r in starts}
    calls = {pid: 0 for pid in requested}
    for line in trace.read_text().splitlines():
        match = re.match(r"\s*(\d+)\s+(?:clock_nanosleep|nanosleep)\(.*?\{tv_sec=(\d+), tv_nsec=(\d+)\}", line)
        if not match:
            if "nanosleep" in line and any(str(pid) == line.split(maxsplit=1)[0] for pid in requested if line.strip()):
                raise ValueError("incomplete worker sleep syscall trace")
            continue
        pid, seconds, nanos = map(int, match.groups())
        if pid in requested:
            requested[pid] += seconds * 1_000_000_000 + nanos
            calls[pid] += 1
    return {"identity": {k: receipt["identity"][k] for k in IDENTITY},
            "provenance": {"commit": receipt["identity"]["commit"], "recorded_at": receipt["identity"]["started_at"], "run_id": receipt["identity"]["run_id"]},
            "geometry": {"spawns": 20, "window": "helper spawn through first Ready frame; no package installation or guest execution"},
            "rows": {"startup/requested_sleep_ns": statistics.median(requested.values()),
                     "startup/sleep_calls": statistics.median(calls.values())},
            "spawn_to_ready_median_ns_diagnostic": statistics.median(r["spawn_to_ready_ns"] for r in starts),
            "sleep_requests_ns": requested}


def regressions(baseline: dict, candidate: dict, mode: str) -> list[tuple]:
    if any(not isinstance(candidate.get("identity", {}).get(k), str) or not candidate["identity"][k].strip() for k in IDENTITY):
        raise ValueError("missing performance identity")
    for key in ("identity", "geometry") + (("cpu",) if mode == "instructions" else ()):
        if not candidate.get(key) or candidate[key] != baseline.get(key):
            raise ValueError(f"incomparable {mode} baseline: {key}")
    if not candidate.get("rows") or set(candidate["rows"]) != set(baseline.get("rows", {})):
        raise ValueError("incomplete performance row population")
    failures = []
    for name, actual in candidate["rows"].items():
        base = baseline["rows"][name]
        if not all(isinstance(v, (int, float)) and math.isfinite(v) and v >= 0 for v in (base, actual)):
            raise ValueError(f"invalid metric {name}")
        ceiling = base * 1.01 if mode == "instructions" else baseline["ceilings"][name]
        if actual > ceiling:
            failures.append((name, base, actual, ceiling))
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("instructions", "startup", "history"))
    parser.add_argument("--binary", required=True)
    parser.add_argument("--worker", required=True)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--calibrate", action="store_true")
    parser.add_argument("--no-record", action="store_true")
    parser.add_argument("--history", type=Path, default=ROOT / ".benchmarks/perf-guard/runtime-duration-history-full.jsonl")
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    committed = ROOT / f"docs/perf/baselines/{args.mode}.json"
    previous_main = Path("/workspace/kiln/lash/perf-gates") / f"{args.mode}-baseline.json"
    baseline_path = args.baseline or (previous_main if previous_main.exists() and not args.calibrate else committed)
    env = {**os.environ, "LASH_VM_WORKER": args.worker, "GITHUB_RUN_ID": uuid.uuid4().hex}
    producer = startup if args.mode == "startup" else instructions
    samples = []
    try:
        if args.mode == "history":
            head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
            main = subprocess.check_output(["git", "ls-remote", "origin", "refs/heads/main"], cwd=ROOT, text=True).split()[0]
            dirty = subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], cwd=ROOT, text=True).strip()
            env["GITHUB_SHA"] = head
            if head != main or dirty:
                raise ValueError("history recording requires the clean current main head")
        for index in range(5 if args.calibrate and args.mode == "startup" else 1):
            directory = args.out / f"run-{index}"
            directory.mkdir(exist_ok=False)
            sample = producer(args.binary, directory, env)
            if args.mode == "history":
                # Keep only counted samples; producer timing tables and raw
                # duration fields do not enter retained history artifacts.
                for child in directory.iterdir():
                    if child.is_dir(): shutil.rmtree(child)
                    else: child.unlink()
            (directory / "sample.json").write_text(json.dumps(sample, indent=2) + "\n")
            samples.append(sample)
            print(f"{args.mode}: collected run {index + 1}", flush=True)
        candidate = samples[-1]
        if args.mode == "history":
            execute([args.binary, "counted-history-record", "--sample", str(args.out / "run-0/sample.json"),
                "--history", str(args.history)], args.out / "history.log", env)
            print((args.out / "history.log").read_text(), end="")
            return 0
        if args.calibrate:
            if any(not isinstance(candidate.get("identity", {}).get(k), str) or not candidate["identity"][k].strip() for k in IDENTITY):
                raise ValueError("missing calibration identity")
            for sample in samples[1:]:
                # Check identity/population without judging calibration noise.
                for key in ("identity", "geometry", "cpu"):
                    if sample.get(key) != samples[0].get(key):
                        raise ValueError(f"calibration changed {key}")
            candidate["rows"] = {k: max(s["rows"][k] for s in samples) for k in candidate["rows"]}
            candidate["noise"] = {k: max(s["rows"][k] for s in samples) - min(s["rows"][k] for s in samples) for k in candidate["rows"]}
            candidate["ceilings"] = {k: v + candidate["noise"][k] for k, v in candidate["rows"].items()}
            baseline_path.write_text(json.dumps(candidate, indent=2) + "\n")
            print(f"{args.mode}: calibrated {baseline_path}")
            return 0
        baseline = read(baseline_path)
        if baseline.get("commit"):
            subprocess.run(["git", "merge-base", "--is-ancestor", baseline["commit"], "HEAD"], cwd=ROOT, check=True)
        failures = regressions(baseline, candidate, args.mode)
        (args.out / "decision.json").write_text(json.dumps({
            "mode": args.mode, "baseline_path": str(baseline_path),
            "baseline": baseline, "candidate": candidate,
            "instruction_relative_threshold": 0.01 if args.mode == "instructions" else None,
            "regressed_rows": failures, "outcome": "fail" if failures else "pass",
        }, indent=2) + "\n")
        print("| Row | Baseline | Observed | Ceiling |\n|---|---:|---:|---:|")
        for name, base, actual, ceiling in failures:
            print(f"| {name} | {base:.3f} | {actual:.3f} | {ceiling:.3f} |")
        if failures:
            print(f"{args.mode}: FAIL ({len(failures)} regressed rows)")
            return 1
        print(f"{args.mode}: PASS ({len(candidate['rows'])} rows)")
        # Only a clean main run advances the previous-main baseline, and a
        # regression/refusal never replaces its own reference.
        if not args.no_record and not args.baseline:
            head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
            main = subprocess.check_output(["git", "rev-parse", "origin/main"], cwd=ROOT, text=True).strip()
            dirty = subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], cwd=ROOT, text=True).strip()
            if head == main and not dirty:
                path = Path("/workspace/kiln/lash/perf-gates") / f"{args.mode}-baseline.json"
                path.parent.mkdir(parents=True, exist_ok=True)
                candidate["commit"] = head
                candidate["ceilings"] = read(baseline_path).get("ceilings", {})
                temporary = path.with_suffix(".tmp")
                temporary.write_text(json.dumps(candidate, indent=2) + "\n")
                temporary.replace(path)
        return 0
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        (args.out / "decision.json").write_text(json.dumps({
            "mode": args.mode, "baseline_path": str(baseline_path), "outcome": "refused", "reason": str(error),
        }, indent=2) + "\n")
        print(f"{args.mode}: REFUSED: {error}")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
