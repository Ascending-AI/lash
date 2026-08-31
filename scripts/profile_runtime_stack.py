#!/usr/bin/env python3
"""Run runtime perf scenarios across Tokio worker stack sizes.

This is a crash-boundary harness for embedded Tokio stack regressions. Each
stack size runs in a fresh process with the lash-perf runtime benchmark Tokio
runtime builder knob, so stack overflows are recorded as failed samples instead
of killing the whole matrix.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path


DEFAULT_STACKS = [
    64 * 1024,
    80 * 1024,
    96 * 1024,
    128 * 1024,
    192 * 1024,
    256 * 1024,
    384 * 1024,
    512 * 1024,
    768 * 1024,
    1024 * 1024,
    1536 * 1024,
    2 * 1024 * 1024,
    3 * 1024 * 1024,
    4 * 1024 * 1024,
    6 * 1024 * 1024,
    8 * 1024 * 1024,
]

DEFAULT_BUDGET_FILE = Path(__file__).resolve().with_name("perf_guard_budgets.json")
STACK_OVERFLOW_MARKERS = (
    "stack overflow",
    "fatal runtime error",
    "aborted (core dumped)",
)

RUNTIME_SCENARIOS_RS = (
    Path(__file__).resolve().parent.parent
    / "crates"
    / "lash-perf"
    / "src"
    / "runtime_perf"
    / "scenarios.rs"
)


def load_known_runtime_scenarios() -> list[str]:
    scenarios_rs = RUNTIME_SCENARIOS_RS.read_text()
    scenarios = re.findall(
        r"runtime_perf_metadata!\(\s*[A-Za-z0-9_]+,\s*\"([a-z0-9_]+)\"",
        scenarios_rs,
    )
    if not scenarios:
        scenarios = re.findall(r'"([a-z0-9_]+)" => Some\(Self::', scenarios_rs)
    if not scenarios:
        raise RuntimeError(f"no runtime perf scenarios found in {RUNTIME_SCENARIOS_RS}")
    return scenarios


KNOWN_RUNTIME_SCENARIOS = load_known_runtime_scenarios()
DEFAULT_SCENARIOS = ["deep_turn_composition"]


def parse_size(value: str) -> int:
    raw = value.strip().lower().replace("_", "")
    multiplier = 1
    for suffix, factor in (
        ("kib", 1024),
        ("kb", 1024),
        ("k", 1024),
        ("mib", 1024 * 1024),
        ("mb", 1024 * 1024),
        ("m", 1024 * 1024),
    ):
        if raw.endswith(suffix):
            raw = raw[: -len(suffix)]
            multiplier = factor
            break
    try:
        return int(raw) * multiplier
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"invalid stack size `{value}`") from exc


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--binary",
        type=Path,
        help="Path to lash-perf. Defaults to target/release/lash-perf or target/debug/lash-perf.",
    )
    parser.add_argument(
        "--build",
        dest="build",
        action="store_true",
        default=True,
        help="Build lash-perf before running the matrix (default: enabled).",
    )
    parser.add_argument(
        "--no-build",
        dest="build",
        action="store_false",
        help="Skip rebuilding and use the existing binary.",
    )
    parser.add_argument(
        "--release",
        action="store_true",
        help="Use target/release/lash-perf instead of target/debug/lash-perf.",
    )
    parser.add_argument(
        "--scenario",
        action="append",
        default=[],
        help=(
            "Runtime perf scenario to run. Defaults to the stack-sensitive runtime "
            "coverage set. May be repeated."
        ),
    )
    parser.add_argument(
        "--stack-bytes",
        action="append",
        type=parse_size,
        default=[],
        help="Worker stack size to test, e.g. 768k, 2m, 4m. May be repeated.",
    )
    parser.add_argument("--runs", type=int, default=1, help="Measured runs per sample.")
    parser.add_argument("--warmups", type=int, default=0, help="Warmups per sample.")
    parser.add_argument("--turns", type=int, default=1, help="Turns per measured run.")
    parser.add_argument(
        "--timeout-seconds",
        type=int,
        default=120,
        help="Timeout for each stack-size sample.",
    )
    parser.add_argument(
        "--out",
        type=Path,
        help="Write matrix JSON here. Defaults under .benchmarks/runtime-stack/.",
    )
    parser.add_argument(
        "--strict-failures",
        action="store_true",
        help=(
            "Exit non-zero when any stack-size sample fails. By default the matrix "
            "exits successfully if every scenario has at least one passing stack size."
        ),
    )
    parser.add_argument(
        "--budget-file",
        type=Path,
        default=DEFAULT_BUDGET_FILE,
        help="Checked-in stack budgets used by --enforce-budgets.",
    )
    parser.add_argument(
        "--enforce-budgets",
        action="store_true",
        help="Exit non-zero unless every selected scenario passes at its checked-in stack budget.",
    )
    parser.add_argument(
        "--budget-only",
        action="store_true",
        help="Run only each scenario's checked-in stack budget (requires --enforce-budgets).",
    )
    parser.add_argument(
        "--cargo-feature",
        action="append",
        default=[],
        help="Additional Cargo feature to enable when building lash-perf.",
    )
    return parser.parse_args()


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def default_out(root: Path) -> Path:
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return root / ".benchmarks" / "runtime-stack" / f"runtime-stack-{stamp}.json"


def resolve_binary(args: argparse.Namespace, root: Path) -> Path:
    if args.binary:
        return args.binary
    profile = "release" if args.release else "debug"
    return cargo_target_dir(root) / profile / "lash-perf"


def cargo_target_dir(root: Path) -> Path:
    value = os.environ.get("CARGO_TARGET_DIR")
    if value:
        path = Path(value)
        return path if path.is_absolute() else root / path
    return root / "target"


def maybe_build(args: argparse.Namespace, root: Path) -> None:
    if not args.build:
        return
    cmd = ["cargo", "build", "-q", "-p", "lash-perf"]
    features = list(args.cargo_feature)
    if features:
        cmd.extend(["--features", ",".join(features)])
    if args.release:
        cmd.append("--release")
    print(f"Building lash-perf binary: {' '.join(cmd)}", file=sys.stderr)
    subprocess.run(cmd, cwd=root, check=True)


def file_sha256(path: Path) -> str | None:
    if not path.exists():
        return None
    hasher = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def binary_metadata(binary: Path) -> dict[str, object]:
    stat = binary.stat() if binary.exists() else None
    return {
        "path": str(binary),
        "exists": binary.exists(),
        "size_bytes": stat.st_size if stat else None,
        "mtime_ns": stat.st_mtime_ns if stat else None,
        "sha256": file_sha256(binary),
    }


def git_metadata(root: Path) -> dict[str, object]:
    def run_git(args: list[str]) -> str | None:
        proc = subprocess.run(
            ["git", *args],
            cwd=root,
            capture_output=True,
            text=True,
            check=False,
        )
        if proc.returncode != 0:
            return None
        return proc.stdout.strip()

    status = run_git(["status", "--porcelain"])
    return {
        "sha": run_git(["rev-parse", "HEAD"]),
        "dirty": bool(status),
    }


def result_path(out: Path, scenario: str, stack_bytes: int) -> Path:
    safe_scenario = scenario.replace("/", "_")
    return out.with_name(f"{out.stem}-{safe_scenario}-{stack_bytes}.runtime-perf.json")


def report_metadata(
    sample_out: Path,
    *,
    scenario: str,
    stack_bytes: int,
) -> dict[str, object]:
    if not sample_out.exists():
        return {
            "status": "failed",
            "failure_reason": "missing_runtime_perf_report",
            "reported_worker_stack_bytes": None,
            "stack_accounted": False,
            "summary_scenarios": [],
        }

    try:
        payload = json.loads(sample_out.read_text())
    except json.JSONDecodeError as exc:
        return {
            "status": "failed",
            "failure_reason": "malformed_runtime_perf_report",
            "json_error": str(exc),
            "reported_worker_stack_bytes": None,
            "stack_accounted": False,
            "summary_scenarios": [],
        }

    if not isinstance(payload, dict):
        return {
            "status": "failed",
            "failure_reason": "non_object_runtime_perf_report",
            "reported_worker_stack_bytes": None,
            "stack_accounted": False,
            "summary_scenarios": [],
        }

    reported_stack_bytes = payload.get("worker_stack_bytes")
    stack_profile = payload.get("stack_profile")
    profile_stack_bytes = None
    if isinstance(stack_profile, dict):
        profile_stack_bytes = stack_profile.get("worker_stack_bytes")
    summaries = payload.get("summary")
    summary_scenarios: list[str] = []
    summary_stack_accounted = False
    if isinstance(summaries, list):
        summary_scenarios = [
            item.get("scenario")
            for item in summaries
            if isinstance(item, dict) and isinstance(item.get("scenario"), str)
        ]
        for item in summaries:
            if not isinstance(item, dict) or item.get("scenario") != scenario:
                continue
            item_stack_profile = item.get("stack_profile")
            summary_stack_accounted = (
                isinstance(item_stack_profile, dict)
                and item_stack_profile.get("worker_stack_bytes") == stack_bytes
            )
            break
    results = payload.get("results")
    result_stack_accounted = False
    if isinstance(results, list):
        for item in results:
            if not isinstance(item, dict) or item.get("scenario") != scenario:
                continue
            item_stack_profile = item.get("stack_profile")
            result_stack_accounted = (
                isinstance(item_stack_profile, dict)
                and item_stack_profile.get("worker_stack_bytes") == stack_bytes
            )
            break
    stack_accounted = (
        reported_stack_bytes == stack_bytes
        and profile_stack_bytes == stack_bytes
        and summary_stack_accounted
        and result_stack_accounted
    )
    scenario_reported = scenario in summary_scenarios
    failure_reasons = []
    if reported_stack_bytes != stack_bytes:
        failure_reasons.append("top_level_stack_size_not_accounted")
    if profile_stack_bytes != stack_bytes:
        failure_reasons.append("stack_profile_size_not_accounted")
    if not summary_stack_accounted:
        failure_reasons.append("summary_stack_size_not_accounted")
    if not result_stack_accounted:
        failure_reasons.append("result_stack_size_not_accounted")
    if not stack_accounted:
        failure_reasons.append("stack_size_not_accounted")
    if not scenario_reported:
        failure_reasons.append("missing_runtime_perf_scenario")

    metadata: dict[str, object] = {
        "status": "ok" if not failure_reasons else "failed",
        "reported_worker_stack_bytes": reported_stack_bytes,
        "reported_stack_profile": stack_profile if isinstance(stack_profile, dict) else None,
        "summary_stack_accounted": summary_stack_accounted,
        "result_stack_accounted": result_stack_accounted,
        "stack_accounted": stack_accounted,
        "summary_scenarios": summary_scenarios,
    }
    if failure_reasons:
        metadata["failure_reason"] = ",".join(failure_reasons)
        metadata["failure_reasons"] = failure_reasons
    return metadata


def contains_stack_overflow(stdout: str, stderr: str) -> bool:
    combined = f"{stdout}\n{stderr}".lower()
    return any(marker in combined for marker in STACK_OVERFLOW_MARKERS)


def run_sample(
    *,
    root: Path,
    binary: Path,
    scenario: str,
    stack_bytes: int,
    out: Path,
    runs: int,
    warmups: int,
    turns: int,
    timeout_seconds: int,
) -> dict[str, object]:
    sample_out = result_path(out, scenario, stack_bytes)
    cmd = [
        str(binary),
        f"--runtime-perf-runs={max(runs, 1)}",
        f"--runtime-perf-warmups={max(warmups, 0)}",
        f"--runtime-perf-turns={max(turns, 1)}",
        f"--runtime-perf-worker-stack-bytes={stack_bytes}",
        "--runtime-perf-scenario",
        scenario,
        f"--runtime-perf-out={sample_out}",
    ]
    started = dt.datetime.now(dt.timezone.utc)
    try:
        proc = subprocess.run(
            cmd,
            cwd=root,
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
        )
    except subprocess.TimeoutExpired as exc:
        return {
            "scenario": scenario,
            "stack_bytes": stack_bytes,
            "status": "timeout",
            "returncode": None,
            "started_at": started.isoformat(),
            "duration_ms": timeout_seconds * 1000,
            "runtime_perf_out": str(sample_out),
            "stdout_tail": (exc.stdout or "")[-4000:],
            "stderr_tail": (exc.stderr or "")[-4000:],
        }

    finished = dt.datetime.now(dt.timezone.utc)
    metadata: dict[str, object] = {}
    status = "failed"
    if proc.returncode == 0:
        metadata = report_metadata(sample_out, scenario=scenario, stack_bytes=stack_bytes)
        status = str(metadata.pop("status"))
    if contains_stack_overflow(proc.stdout or "", proc.stderr or ""):
        reasons = list(metadata.get("failure_reasons", []))
        if "stack_overflow" not in reasons:
            reasons.append("stack_overflow")
        metadata["failure_reasons"] = reasons
        metadata["failure_reason"] = ",".join(reasons)
        status = "failed"

    sample_failed = status != "ok"
    sample = {
        "scenario": scenario,
        "stack_bytes": stack_bytes,
        "status": status,
        "returncode": proc.returncode,
        "started_at": started.isoformat(),
        "duration_ms": round((finished - started).total_seconds() * 1000, 3),
        "runtime_perf_out": str(sample_out),
        "stdout_tail": proc.stdout[-4000:] if sample_failed else "",
        "stderr_tail": proc.stderr[-4000:] if sample_failed else "",
    }
    sample.update(metadata)
    return sample


def first_success(samples: list[dict[str, object]], scenario: str) -> int | None:
    for sample in sorted(
        (sample for sample in samples if sample["scenario"] == scenario),
        key=lambda sample: int(sample["stack_bytes"]),
    ):
        summary_scenarios = sample.get("summary_scenarios", [])
        if (
            sample["status"] == "ok"
            and sample.get("stack_accounted") is True
            and isinstance(summary_scenarios, list)
            and scenario in summary_scenarios
        ):
            return int(sample["stack_bytes"])
    return None


def load_stack_budgets(path: Path, scenarios: list[str]) -> dict[str, int]:
    try:
        payload = json.loads(path.read_text())
    except FileNotFoundError as exc:
        raise SystemExit(f"error: stack budget file not found: {path}") from exc
    except json.JSONDecodeError as exc:
        raise SystemExit(f"error: invalid stack budget JSON at {path}: {exc}") from exc
    configured = payload.get("runtime_stack", {})
    budgets: dict[str, int] = {}
    for scenario in scenarios:
        entry = configured.get(scenario) if isinstance(configured, dict) else None
        value = entry.get("budget_bytes") if isinstance(entry, dict) else None
        if not isinstance(value, int) or value <= 0:
            raise SystemExit(
                f"error: missing positive runtime_stack.{scenario}.budget_bytes in {path}"
            )
        budgets[scenario] = value
    return budgets


def budget_sample_passed(
    samples: list[dict[str, object]], scenario: str, budget_bytes: int
) -> bool:
    return any(
        sample["scenario"] == scenario
        and sample["stack_bytes"] == budget_bytes
        and sample["status"] == "ok"
        and sample.get("stack_accounted") is True
        for sample in samples
    )


def main() -> int:
    args = parse_args()
    root = repo_root()
    out = args.out or default_out(root)
    out.parent.mkdir(parents=True, exist_ok=True)
    binary = resolve_binary(args, root)
    scenarios = args.scenario or DEFAULT_SCENARIOS
    if args.budget_only and not args.enforce_budgets:
        raise SystemExit("error: --budget-only requires --enforce-budgets")
    stack_budgets = (
        load_stack_budgets(args.budget_file, scenarios) if args.enforce_budgets else {}
    )
    if args.budget_only:
        stacks = sorted(set(stack_budgets.values()))
    else:
        stacks = sorted(set(args.stack_bytes or DEFAULT_STACKS) | set(stack_budgets.values()))

    maybe_build(args, root)
    if not binary.exists():
        raise SystemExit(f"error: binary not found: {binary}")

    samples: list[dict[str, object]] = []
    for scenario in scenarios:
        for stack_bytes in stacks:
            print(f"stack={stack_bytes} scenario={scenario}", file=sys.stderr)
            sample = run_sample(
                root=root,
                binary=binary,
                scenario=scenario,
                stack_bytes=stack_bytes,
                out=out,
                runs=args.runs,
                warmups=args.warmups,
                turns=args.turns,
                timeout_seconds=args.timeout_seconds,
            )
            samples.append(sample)
            print(f"  -> {sample['status']} rc={sample['returncode']}", file=sys.stderr)

    payload = {
        "created_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "kind": "runtime-stack",
        "binary": str(binary),
        "binary_metadata": binary_metadata(binary),
        "git": git_metadata(root),
        "release": args.release,
        "cargo_features": args.cargo_feature,
        "runs": max(args.runs, 1),
        "warmups": max(args.warmups, 0),
        "turns": max(args.turns, 1),
        "scenarios": scenarios,
        "known_scenarios": KNOWN_RUNTIME_SCENARIOS,
        "missing_known_scenarios": sorted(set(KNOWN_RUNTIME_SCENARIOS) - set(scenarios)),
        "stack_bytes": stacks,
        "stack_budgets": stack_budgets,
        "first_success_stack_bytes": {
            scenario: first_success(samples, scenario) for scenario in scenarios
        },
        "samples": samples,
    }
    payload["budget_results"] = {
        scenario: budget_sample_passed(samples, scenario, budget)
        for scenario, budget in stack_budgets.items()
    }
    out.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
    print(f"Runtime stack matrix: {out}", file=sys.stderr)
    print(json.dumps(payload, indent=2, sort_keys=True))
    if args.enforce_budgets and not all(payload["budget_results"].values()):
        return 1
    if args.strict_failures:
        return 0 if all(sample["status"] == "ok" for sample in samples) else 1
    return 0 if all(first_success(samples, scenario) is not None for scenario in scenarios) else 1


if __name__ == "__main__":
    raise SystemExit(main())
