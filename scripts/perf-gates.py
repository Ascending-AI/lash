#!/usr/bin/env python3
"""Local counted performance checks with bounded, independently reported rows."""
from __future__ import annotations

import argparse
import hashlib
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

from perf_gate_baseline import (calibrate_changed, check_sources, geometry_key,
                                kernel_geometry, kernel_workloads, references)

ROOT = Path(__file__).resolve().parents[1]
ROW_CAP_SECONDS = 1200
IDENTITY = ("host", "compiler", "allocator", "build", "build_profile")


def execute(command: list[str], log: Path, env: dict[str, str]) -> None:
    with log.open("w") as output:
        result = subprocess.run(command, cwd=ROOT, env=env, stdout=output, stderr=subprocess.STDOUT, timeout=1200)
    if result.returncode:
        raise ValueError(f"producer failed ({result.returncode}); see {log}")


def execute_row(command: list[str], log: Path, env: dict[str, str]) -> dict | None:
    """Bound one producer, retaining whole-process counts if it cannot finish."""
    counts = log.with_suffix(".instructions.csv")
    with log.open("w") as output:
        result = subprocess.run([
            "perf", "stat", "--no-big-num", "--no-scale", "-x", ";", "-o", str(counts),
            "-e", "instructions:u", "--", "timeout", "--verbose", "--signal=TERM", "--kill-after=5",
            str(ROW_CAP_SECONDS), *command,
        ], cwd=ROOT, env=env, stdout=output, stderr=subprocess.STDOUT,
            timeout=ROW_CAP_SECONDS + 30)
    if result.returncode:
        exceeded = result.returncode == 124 or (result.returncode == 137 and "timeout: sending signal" in log.read_text())
        fields = next((line.split(";") for line in counts.read_text().splitlines()
                       if ";instructions:u;" in line), [])
        count = int(fields[0]) if fields and fields[0].isdigit() else None
        return {"reason": "exceeded cap" if exceeded else f"producer failed ({result.returncode}); see {log}",
                "cap_seconds": ROW_CAP_SECONDS if exceeded else None, "instructions": count,
                "instruction_scope": "whole producer process tree including setup and warmups; not a slope",
                "pmu_running_percent": float(fields[4]) if count is not None else None}
    return None


def read(path: Path) -> dict:
    return json.loads(path.read_text())


def instructions(binary: str, directory: Path, env: dict[str, str]) -> dict:
    workloads_by_name = kernel_workloads(ROOT)
    captures = []
    row_failures = {}
    for workload, (low, _, _) in workloads_by_name.items():
        factor, runs = ("100", "5") if low < 10000 else ("10", "1" if low >= 20000 else "5")
        for dialect in ("typescript", "python"):
            name = f"kernel/{dialect}/{workload}"
            compare = directory / f"compare-{dialect}-{workload}.json"
            failure = execute_row([binary, "compare", "--out", str(compare), "--instructions",
                                   "--workload", workload, "--dialect", dialect,
                                   "--runs", runs, "--warmups", "1", "--factor", factor],
                                  compare.with_suffix(".log"), env)
            if failure:
                row_failures[name] = failure
                print(f"{name}: FAIL {failure['reason']}; instructions={failure['instructions']}", flush=True)
                continue
            capture = read(compare)
            if capture.get("errors") or len(capture.get("rows", [])) != 1:
                raise ValueError(f"incomplete kernel comparison: {name}")
            if captures and capture["cpu"] != captures[0]["cpu"]:
                raise ValueError("kernel CPU affinity changed between populations")
            captures.append(capture)
    kernel = captures[0] if captures else {
        "cpu": f"pinned:{min(os.sched_getaffinity(0))}", "commit": env.get("GITHUB_SHA", ""),
        "recorded_at": "", "run_id": env.get("GITHUB_RUN_ID", ""),
    }
    kernel["rows"] = [row for capture in captures for row in capture["rows"]]
    rows = {}
    geometry = kernel_geometry(ROOT)
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
        geometry[name].update(runs=len(row["samples"]), warmups=kernel["geometry"]["warmups"])
        artifacts["kernel"] = row["engine_identity"]
    expected_kernel = {f"kernel/{dialect}/{name}" for dialect in ("typescript", "python")
                       for name in workloads_by_name}
    if set(rows) | set(row_failures) != expected_kernel:
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
            failure = execute_row(command, directory / f"{name}.log", env)
            if failure:
                source = read(ROOT / f"crates/lash-perf/workloads/replay/{name}.json")
                processes = ("host",) if source["protocol"] == "standard" else ("host", "worker")
                for process in processes:
                    row_failures[f"replay/{name}/{process}"] = failure
                print(f"replay/{name}: FAIL {failure['reason']}; instructions={failure['instructions']}", flush=True)
                continue
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
    return {"identity": identity, "cpu": kernel["cpu"], "geometry": geometry, "rows": rows, "provenance": provenance, "artifacts": artifacts, "counted_metrics": counts, "experiments": experiments, "row_failures": row_failures}


def startup(binary: str, directory: Path, env: dict[str, str]) -> dict:
    out, trace = directory / "startup.json", directory / "startup.strace"
    failure = execute_row(["strace", "-f", "-qq", "-s", "256", "-e", "trace=clock_nanosleep,nanosleep", "-o", str(trace), binary, "vm-startup-gate", "--out", str(out)], directory / "startup.log", env)
    if failure:
        return {"identity": None, "rows": {}, "row_failures": {
            name: failure for name in ("startup/requested_sleep_ns", "startup/sleep_calls")}}
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
    """Judge comparable rows. Instructions resolve changed references before calling."""
    if any(not isinstance((candidate.get("identity") or {}).get(k), str) or not candidate["identity"][k].strip() for k in IDENTITY):
        raise ValueError("missing performance identity")
    for key in ("identity", "cpu") if mode == "instructions" else ("identity", "geometry"):
        if not candidate.get(key) or candidate[key] != baseline.get(key):
            raise ValueError(f"incomparable {mode} baseline: {key}")
    if not (candidate.get("rows") or candidate.get("row_failures")) or (mode != "instructions" and set(candidate["rows"]) != set(baseline.get("rows", {}))):
        raise ValueError("incomplete performance row population")
    failures = []
    for name, actual in candidate["rows"].items():
        if mode == "instructions":
            key = geometry_key(name)
            if name not in baseline["rows"] or candidate["geometry"].get(key) != baseline["geometry"].get(key):
                continue
        base = baseline["rows"][name]
        if not all(isinstance(v, (int, float)) and math.isfinite(v) and v >= 0 for v in (base, actual)):
            raise ValueError(f"invalid metric {name}")
        ceiling = base * 1.01 if mode == "instructions" else baseline["ceilings"][name]
        if actual > ceiling:
            failures.append((name, base, actual, ceiling))
    return failures


def report_rows(candidate: dict, selected: dict, failures: list, mode: str, refusal: str | None = None) -> None:
    print("| Row | Verdict | Baseline | Observed | Ceiling |\n|---|---|---:|---:|---:|")
    capped = candidate.get('row_failures', {})
    failed_names = {row[0] for row in failures}
    for name, actual in candidate["rows"].items():
        base = selected["rows"][name]
        ceiling = base * 1.01 if mode == "instructions" else selected["ceilings"][name]
        verdict = refusal or selected.get("row_verdicts", {}).get(name, "gated")
        print(f"| {name} | {'REFUSED' if refusal else 'FAIL' if name in failed_names else 'PASS'}; {verdict} | {base:.3f} | {actual:.3f} | {ceiling:.3f} |")
    for name, failure in capped.items():
        base = selected["rows"][name]
        print(f"| {name} | FAIL; {failure['reason']} {failure['cap_seconds'] or ''} | {base:.3f} | - | instructions={failure['instructions']} (whole process) |")
    for name in selected["rows"].keys() - candidate["rows"].keys() - capped.keys():
        print(f"| {name} | {selected['row_verdicts'][name]} | {selected['rows'][name]:.3f} | - | - |")


def binary_identity(binary: str, worker: str) -> dict:
    identity = {}
    for name, path in (("perf", binary), ("worker", worker)):
        with Path(path).open("rb") as executable:
            identity[name] = hashlib.file_digest(executable, "sha256").hexdigest()
    return identity


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("instructions", "startup", "history", "check-sources"))
    parser.add_argument("--binary")
    parser.add_argument("--worker")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--sample", type=Path, help="reuse this same-commit instruction sample for history")
    parser.add_argument("--calibrate", action="store_true")
    parser.add_argument("--no-record", action="store_true")
    parser.add_argument("--history", type=Path, default=ROOT / ".benchmarks/perf-guard/runtime-duration-history-full.jsonl")
    args = parser.parse_args()
    if args.mode == "check-sources":
        try:
            check_sources(ROOT)
        except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
            print(f"instructions: REFUSED: {error}")
            return 2
        print("instructions: workload sources match committed baseline")
        return 0
    if not args.binary or not args.worker or args.out is None:
        parser.error("--binary, --worker and --out are required for measurements")
    args.out.mkdir(parents=True, exist_ok=True)
    committed = ROOT / f"docs/perf/baselines/{args.mode}.json"
    previous_main = Path("/workspace/kiln/lash/perf-gates") / f"{args.mode}-baseline.json"
    baseline_path = args.baseline or (previous_main if previous_main.exists() and not args.calibrate else committed)
    env = {**os.environ, "LASH_VM_WORKER": args.worker, "GITHUB_RUN_ID": uuid.uuid4().hex}
    producer = startup if args.mode == "startup" else instructions
    samples = []
    candidate = {}
    try:
        if args.mode == "history":
            subprocess.run(["git", "fetch", "origin", "main"], cwd=ROOT, check=True)
            head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
            on_main = subprocess.run(["git", "merge-base", "--is-ancestor", head, "origin/main"], cwd=ROOT).returncode == 0
            dirty = subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], cwd=ROOT, text=True).strip()
            env["GITHUB_SHA"] = head
            if not on_main or dirty:
                raise ValueError("history recording requires a clean commit landed on main")
        for index in range(5 if args.calibrate and args.mode == "startup" else 1):
            directory = args.out / f"run-{index}"
            directory.mkdir(exist_ok=False)
            if args.sample:
                if args.mode != "history":
                    raise ValueError("--sample is only supported for history")
                sample = read(args.sample)
                if sample["provenance"]["commit"] != head or sample.get("binaries") != binary_identity(args.binary, args.worker):
                    raise ValueError("history sample commit or executables changed")
            else:
                env["GITHUB_SHA"] = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
                sample = producer(args.binary, directory, env)
                sample["binaries"] = binary_identity(args.binary, args.worker)
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
        if args.mode == "history" and not candidate.get("row_failures"):
            execute([args.binary, "counted-history-record", "--sample", str(args.out / "run-0/sample.json"),
                "--history", str(args.history)], args.out / "history.log", env)
            print((args.out / "history.log").read_text(), end="")
            return 0
        if args.calibrate:
            if candidate.get("row_failures"):
                raise ValueError("cannot calibrate capped rows")
            if any(not isinstance((candidate.get("identity") or {}).get(k), str) or not candidate["identity"][k].strip() for k in IDENTITY):
                raise ValueError("missing calibration identity")
            for sample in samples[1:]:
                # Check identity/population without judging calibration noise.
                for key in ("identity", "geometry", "cpu"):
                    if sample.get(key) != samples[0].get(key):
                        raise ValueError(f"calibration changed {key}")
            candidate["rows"] = {k: max(s["rows"][k] for s in samples) for k in candidate["rows"]}
            candidate["noise"] = {k: max(s["rows"][k] for s in samples) - min(s["rows"][k] for s in samples) for k in candidate["rows"]}
            candidate["ceilings"] = {k: v + candidate["noise"][k] for k, v in candidate["rows"].items()}
            if args.mode == "instructions":
                baseline = read(baseline_path)
                failures = regressions(baseline, candidate, args.mode)
                changed = [name for name in candidate["rows"]
                           if name not in baseline["rows"] or baseline["geometry"].get(geometry_key(name)) != candidate["geometry"].get(geometry_key(name))]
                (args.out / "decision.json").write_text(json.dumps({
                    "mode": args.mode, "baseline": baseline, "candidate": candidate,
                    "regressed_rows": failures, "changed_rows": changed,
                    "outcome": "fail" if failures else "calibrated",
                }, indent=2) + "\n")
                for name, base, actual, ceiling in failures:
                    print(f"{name}: FAIL baseline={base:.3f} observed={actual:.3f} ceiling={ceiling:.3f}")
                if failures:
                    print("instructions: FAIL; calibration preserves the unchanged row references")
                candidate = calibrate_changed(baseline, candidate)
                print(f"instructions: recalibrated {len(changed)} changed rows; unchanged references retained")
            baseline_path.write_text(json.dumps(candidate, indent=2) + "\n")
            print(f"{args.mode}: calibrated {baseline_path}")
            return 1 if args.mode == "instructions" and failures else 0
        if args.mode == "history":
            baseline_path = args.baseline or Path("/workspace/kiln/lash/perf-gates/instructions-baseline.json")
            committed = ROOT / "docs/perf/baselines/instructions.json"
            if not baseline_path.exists():
                baseline_path = committed
        comparison_mode = "instructions" if args.mode == "history" else args.mode
        baseline = read(baseline_path)
        if baseline.get("commit"):
            subprocess.run(["git", "merge-base", "--is-ancestor", baseline["commit"], "HEAD"], cwd=ROOT, check=True)
        selected = references(baseline, candidate, read(committed)) if comparison_mode == "instructions" else baseline
        failures = regressions(selected, candidate, comparison_mode)
        capped = candidate.get("row_failures", {})
        (args.out / "decision.json").write_text(json.dumps({
            "mode": args.mode, "baseline_path": str(baseline_path),
            "baseline": baseline, "candidate": candidate,
            "row_verdicts": selected.get("row_verdicts", {}),
            "selected_baseline": selected,
            "instruction_relative_threshold": 0.01 if comparison_mode == "instructions" else None,
            "regressed_rows": failures, "row_failures": capped, "outcome": "fail" if failures or capped else "pass",
        }, indent=2) + "\n")
        report_rows(candidate, selected, failures, comparison_mode)
        if failures or capped:
            print(f"{args.mode}: FAIL ({len(failures)} regressed rows, {len(capped)} failed producers)")
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
        if candidate.get("row_failures"):
            # An unavailable comparison identity must not hide already capped
            # rows or manufacture passing verdicts for completed observations.
            baseline = read(baseline_path)
            (args.out / "decision.json").write_text(json.dumps({
                "mode": args.mode, "baseline_path": str(baseline_path), "baseline": baseline,
                "candidate": candidate, "selected_baseline": baseline, "regressed_rows": [],
                "row_failures": candidate["row_failures"], "comparison_refusal": str(error), "outcome": "fail",
            }, indent=2) + "\n")
            report_rows(candidate, baseline, [], "instructions", refusal=str(error))
            print(f"{args.mode}: FAIL (capped rows; remaining comparisons refused: {error})")
            return 1
        (args.out / "decision.json").write_text(json.dumps({
            "mode": args.mode, "baseline_path": str(baseline_path), "outcome": "refused", "reason": str(error),
        }, indent=2) + "\n")
        print(f"{args.mode}: REFUSED: {error}")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
