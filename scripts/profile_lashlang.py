#!/usr/bin/env python3
"""Run Lashlang perf/profile sweeps and write a structured JSON report."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

try:
    import resource
except ImportError:  # pragma: no cover - Windows-only fallback.
    resource = None  # type: ignore[assignment]


PERF_MODES = [
    "one_shot",
    "prewarmed_one_shot",
    "link_artifact",
    "compiled_execute",
    "snapshot",
    "artifact_roundtrip",
    "compiled_process_cache",
    "compiled_program_cache",
    "linked_program_cache",
    "phase_breakdown",
]

DEFAULT_PERF_MODES = [
    "one_shot",
    "prewarmed_one_shot",
    "link_artifact",
    "compiled_execute",
    "snapshot",
    "artifact_roundtrip",
    "compiled_process_cache",
    "compiled_program_cache",
    "linked_program_cache",
    "phase_breakdown",
]

DEFAULT_STACK_BUDGET_BYTES = 2 * 1024 * 1024
DEFAULT_BUDGET_FILE = Path(__file__).resolve().with_name("perf_guard_budgets.json")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--scenario",
        action="append",
        default=[],
        help="Scenario to run; defaults to all. Known names are loaded from the benchmark binary.",
    )
    parser.add_argument("--mode", action="append", default=[], help="Perf mode to run; defaults to the standard sweep.")
    parser.add_argument("--iterations", type=int, default=10_000)
    parser.add_argument("--profile-iterations", type=int, default=10_000)
    parser.add_argument(
        "--profile-scenario",
        action="append",
        default=[],
        help="Profile scenario; defaults to aggregate all. Known names are loaded from the benchmark binary.",
    )
    parser.add_argument("--skip-perf", action="store_true")
    parser.add_argument("--skip-profile", action="store_true")
    parser.add_argument("--debug", action="store_true")
    parser.add_argument("--build", dest="build", action="store_true", default=True)
    parser.add_argument("--no-build", dest="build", action="store_false")
    parser.add_argument("--out", type=Path, help="Defaults under .benchmarks/lashlang-perf/.")
    parser.add_argument(
        "--budget-file",
        type=Path,
        default=DEFAULT_BUDGET_FILE,
        help="JSON budget file for --enforce-budgets.",
    )
    parser.add_argument(
        "--enforce-budgets",
        action="store_true",
        help="Exit non-zero when a Lashlang perf guard budget is exceeded.",
    )
    parser.add_argument(
        "--stack-budget-bytes",
        type=int,
        default=None,
        help=(
            "Stack budget to record and apply to Lashlang profiling subprocesses. "
            "Defaults to LASH_STACK_BUDGET_BYTES, LASH_RUST_MIN_STACK_BUDGET, "
            "LASH_STACK_BUDGET_KB, or 2 MiB."
        ),
    )
    return parser.parse_args()


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def default_out(root: Path) -> Path:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return root / ".benchmarks" / "lashlang-perf" / f"{stamp}.json"


def parse_env_stack_bytes(value: str | None) -> int | None:
    if value is None:
        return None
    try:
        parsed = int(value)
    except ValueError:
        return None
    return parsed if parsed > 0 else None


def default_stack_budget_bytes() -> int:
    for name in ("LASH_STACK_BUDGET_BYTES", "LASH_RUST_MIN_STACK_BUDGET"):
        parsed = parse_env_stack_bytes(os.environ.get(name))
        if parsed is not None:
            return parsed
    stack_budget_kb = parse_env_stack_bytes(os.environ.get("LASH_STACK_BUDGET_KB"))
    if stack_budget_kb is not None:
        return stack_budget_kb * 1024
    return DEFAULT_STACK_BUDGET_BYTES


def apply_stack_budget(stack_budget_bytes: int) -> None:
    os.environ.setdefault("RUST_MIN_STACK", str(stack_budget_bytes))
    if resource is None:
        return
    try:
        soft, hard = resource.getrlimit(resource.RLIMIT_STACK)
    except (OSError, ValueError):
        return
    infinity = resource.RLIM_INFINITY
    if soft != infinity and soft <= stack_budget_bytes:
        return
    if hard != infinity and stack_budget_bytes > hard:
        return
    try:
        resource.setrlimit(resource.RLIMIT_STACK, (stack_budget_bytes, hard))
    except (OSError, ValueError):
        return


def process_stack_limits() -> tuple[int | None, int | None, bool]:
    if resource is None:
        return None, None, False
    try:
        soft, hard = resource.getrlimit(resource.RLIMIT_STACK)
    except (OSError, ValueError):
        return None, None, False
    infinity = resource.RLIM_INFINITY
    soft_bytes = None if soft == infinity or soft < 0 else int(soft)
    hard_bytes = None if hard == infinity or hard < 0 else int(hard)
    return soft_bytes, hard_bytes, hard == infinity


def current_stack_profile(stack_budget_bytes: int) -> dict[str, Any]:
    rust_min_stack_bytes = parse_env_stack_bytes(os.environ.get("RUST_MIN_STACK"))
    soft_bytes, hard_bytes, hard_unlimited = process_stack_limits()
    if rust_min_stack_bytes is not None:
        measured_stack_bytes = rust_min_stack_bytes
        measured_stack_source = "rust_min_stack"
    elif soft_bytes is not None:
        measured_stack_bytes = soft_bytes
        measured_stack_source = "process_stack_soft_limit"
    else:
        measured_stack_bytes = None
        measured_stack_source = None
    within_stack_budget = (
        measured_stack_bytes <= stack_budget_bytes
        if measured_stack_bytes is not None
        else None
    )
    return {
        "worker_stack_bytes": None,
        "rust_min_stack_bytes": rust_min_stack_bytes,
        "process_stack_soft_limit_bytes": soft_bytes,
        "process_stack_hard_limit_bytes": hard_bytes,
        "process_stack_hard_limit_unlimited": hard_unlimited,
        "measured_stack_bytes": measured_stack_bytes,
        "measured_stack_source": measured_stack_source,
        "stack_budget_bytes": stack_budget_bytes,
        "within_stack_budget": within_stack_budget,
    }


def resolve_requested(values: list[str], known: list[str], default: list[str]) -> list[str]:
    requested = default if not values else values
    resolved: list[str] = []
    for value in requested:
        if value == "all":
            for item in known:
                if item not in resolved:
                    resolved.append(item)
            continue
        if value not in known:
            expected = ", ".join([*known, "all"])
            raise SystemExit(f"error: unknown value `{value}`; expected one of: {expected}")
        if value not in resolved:
            resolved.append(value)
    return resolved


def resolve_profile_scenarios(values: list[str], known: list[str]) -> list[str]:
    requested = values or ["all"]
    resolved: list[str] = []
    for value in requested:
        if value != "all" and value not in known:
            expected = ", ".join([*known, "all"])
            raise SystemExit(f"error: unknown profile scenario `{value}`; expected one of: {expected}")
        if value not in resolved:
            resolved.append(value)
    return resolved


def maybe_build(root: Path, debug: bool, build: bool) -> None:
    if not build:
        return
    cmd = ["cargo", "build", "-q", "-p", "lashlang", "--example", "perf", "--example", "profile"]
    if not debug:
        cmd.append("--release")
    print(f"Building Lashlang profiling examples: {' '.join(cmd)}", file=sys.stderr)
    subprocess.run(cmd, cwd=root, check=True)


def example_path(root: Path, debug: bool, name: str) -> Path:
    return cargo_target_dir(root) / ("debug" if debug else "release") / "examples" / name


def cargo_target_dir(root: Path) -> Path:
    value = os.environ.get("CARGO_TARGET_DIR")
    if value:
        path = Path(value)
        return path if path.is_absolute() else root / path
    return root / "target"


def run_command(root: Path, cmd: list[str]) -> str:
    proc = subprocess.run(cmd, cwd=root, check=False, capture_output=True, text=True)
    if proc.returncode != 0:
        if proc.stdout:
            print(proc.stdout, file=sys.stderr, end="")
        if proc.stderr:
            print(proc.stderr, file=sys.stderr, end="")
        raise SystemExit(proc.returncode)
    return proc.stdout


def load_scenarios(root: Path, binary: Path) -> list[str]:
    output = run_command(root, [str(binary), "--list-scenarios"])
    scenarios = [line.strip() for line in output.splitlines() if line.strip()]
    if not scenarios:
        raise SystemExit(f"error: benchmark binary did not report scenarios: {binary}")
    return scenarios


def parse_scalar(value: str) -> str | int | float:
    value = value.strip()
    try:
        if "." in value:
            return float(value)
        return int(value)
    except ValueError:
        return value


def parse_perf_output(text: str) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for line in text.splitlines():
        if ": " not in line:
            continue
        key, value = line.split(": ", 1)
        result[key.strip()] = parse_scalar(value)
    return result


def parse_hotspot_line(line: str) -> dict[str, Any] | None:
    parts = line.split()
    if len(parts) < 4:
        return None
    stat: dict[str, Any] = {"name": parts[0]}
    for part in parts[1:]:
        if "=" not in part:
            continue
        key, value = part.split("=", 1)
        stat[key] = parse_scalar(value)
    return stat


def parse_profile_output(text: str) -> dict[str, Any]:
    result: dict[str, Any] = {"instruction_hotspots": [], "builtin_hotspots": []}
    section: str | None = None
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped == "lashlang profile":
            continue
        if stripped == "instruction_hotspots:":
            section = "instruction_hotspots"
            continue
        if stripped == "builtin_hotspots:":
            section = "builtin_hotspots"
            continue
        if section:
            stat = parse_hotspot_line(stripped)
            if stat:
                result[section].append(stat)
            continue
        if ": " in stripped:
            key, value = stripped.split(": ", 1)
            result[key] = parse_scalar(value)
    return result


def git_info(root: Path) -> dict[str, Any]:
    sha = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=root, capture_output=True, text=True).stdout.strip()
    dirty = subprocess.run(["git", "diff", "--quiet", "--ignore-submodules", "--"], cwd=root).returncode != 0
    return {"sha": sha or "unknown", "dirty": dirty}


def load_budget_file(path: Path) -> dict[str, Any]:
    if not path.exists():
        raise SystemExit(f"error: Lashlang budget file not found: {path}")
    try:
        return json.loads(path.read_text())
    except json.JSONDecodeError as exc:
        raise SystemExit(f"error: invalid Lashlang budget JSON at {path}: {exc}") from exc


def budget_value(
    budgets: dict[str, Any],
    section: str,
    scenario: str,
    metric: str,
    mode: str | None = None,
) -> float | None:
    section_budgets = budgets.get("lashlang", {}).get(section, {})
    if not isinstance(section_budgets, dict):
        return None
    scenario_budgets = section_budgets.get(scenario)
    if not isinstance(scenario_budgets, dict):
        scenario_budgets = section_budgets.get("default", {})
    if mode is not None and isinstance(scenario_budgets, dict):
        mode_budgets = scenario_budgets.get(mode)
        if isinstance(mode_budgets, dict):
            scenario_budgets = mode_budgets
    value = scenario_budgets.get(metric) if isinstance(scenario_budgets, dict) else None
    if isinstance(value, int | float):
        return float(value)
    return None


def budget_result(
    *,
    section: str,
    scenario: str,
    mode: str | None,
    metric: str,
    actual: float | None,
    budget: float | None,
    reason: str | None = None,
) -> dict[str, Any]:
    passed = actual is not None and budget is not None and actual <= budget
    if budget is None and reason is None:
        reason = "missing budget"
    if reason:
        passed = False
    return {
        "section": section,
        "scenario": scenario,
        "mode": mode,
        "metric": metric,
        "actual": actual,
        "budget": budget,
        "passed": passed,
        "reason": reason,
    }


def evaluate_lashlang_budgets(report: dict[str, Any], budgets: dict[str, Any]) -> list[dict[str, Any]]:
    results: list[dict[str, Any]] = []
    perf_results = report.get("perf_results", [])
    profile_results = report.get("profile_results", [])
    if not perf_results:
        results.append(
            budget_result(
                section="perf",
                scenario="all",
                mode=None,
                metric="perf_results",
                actual=None,
                budget=1.0,
                reason="missing perf results",
            )
        )
    if not profile_results:
        results.append(
            budget_result(
                section="profile",
                scenario="all",
                mode=None,
                metric="profile_results",
                actual=None,
                budget=1.0,
                reason="missing profile results",
            )
        )

    for row in perf_results:
        scenario = str(row.get("scenario_arg", "unknown"))
        mode = str(row.get("mode_arg", "unknown"))
        for metric in ("allocated_bytes_per_iter", "allocations_per_iter"):
            value = row.get(metric)
            actual = float(value) if isinstance(value, int | float) else None
            results.append(
                budget_result(
                    section="perf",
                    scenario=scenario,
                    mode=mode,
                    metric=metric,
                    actual=actual,
                    budget=budget_value(
                        budgets,
                        "perf",
                        scenario,
                        f"{metric}_max",
                        mode,
                    ),
                    reason=None if actual is not None else f"missing {metric}",
                )
            )

    for row in profile_results:
        scenario = str(row.get("scenario_arg", "unknown"))
        iterations = row.get("iterations")
        if not isinstance(iterations, int | float) or iterations <= 0:
            results.append(
                budget_result(
                    section="profile",
                    scenario=scenario,
                    mode=None,
                    metric="iterations",
                    actual=None,
                    budget=1.0,
                    reason="missing profile iterations",
                )
            )
            continue
        instruction_count = sum(
            hotspot.get("count", 0)
            for hotspot in row.get("instruction_hotspots", [])
            if isinstance(hotspot.get("count", 0), int | float)
        )
        instructions_per_iter = float(instruction_count) / float(iterations)
        results.append(
            budget_result(
                section="profile",
                scenario=scenario,
                mode=None,
                metric="instructions_per_iter",
                actual=instructions_per_iter,
                budget=budget_value(
                    budgets,
                    "profile",
                    scenario,
                    "instructions_per_iter_max",
                ),
            )
        )
    return results


def main() -> int:
    args = parse_args()
    root = repo_root()
    modes = resolve_requested(args.mode, PERF_MODES, DEFAULT_PERF_MODES)
    stack_budget_bytes = (
        args.stack_budget_bytes
        if args.stack_budget_bytes is not None and args.stack_budget_bytes > 0
        else default_stack_budget_bytes()
    )

    maybe_build(root, args.debug, args.build)
    apply_stack_budget(stack_budget_bytes)
    perf_bin = example_path(root, args.debug, "perf")
    profile_bin = example_path(root, args.debug, "profile")

    scenario_binary = perf_bin if perf_bin.exists() else profile_bin
    if not scenario_binary.exists():
        raise SystemExit(
            f"error: no Lashlang profiling example found; expected {perf_bin} or {profile_bin}"
        )
    known_scenarios = load_scenarios(root, scenario_binary)
    scenarios = resolve_requested(args.scenario, known_scenarios, known_scenarios)
    profile_scenarios = resolve_profile_scenarios(args.profile_scenario, known_scenarios)
    stack_profile = current_stack_profile(stack_budget_bytes)

    perf_results = []
    if not args.skip_perf:
        if not perf_bin.exists():
            raise SystemExit(f"error: perf example not found: {perf_bin}")
        for mode in modes:
            iterations = max(args.iterations, 1)
            for scenario in scenarios:
                parsed = parse_perf_output(run_command(root, [str(perf_bin), mode, scenario, str(iterations)]))
                parsed["mode_arg"] = mode
                parsed["scenario_arg"] = scenario
                parsed["stack_profile"] = dict(stack_profile)
                perf_results.append(parsed)

    profile_results = []
    if not args.skip_profile:
        if not profile_bin.exists():
            raise SystemExit(f"error: profile example not found: {profile_bin}")
        for scenario in profile_scenarios:
            parsed = parse_profile_output(run_command(root, [str(profile_bin), scenario, str(max(args.profile_iterations, 1))]))
            parsed["scenario_arg"] = scenario
            parsed["stack_profile"] = dict(stack_profile)
            profile_results.append(parsed)

    out_path = args.out or default_out(root)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    report = {
        "created_at": datetime.now(timezone.utc).isoformat(),
        "kind": "lashlang-perf",
        "git": git_info(root),
        "build_mode": "debug" if args.debug else "release",
        "stack_profile": stack_profile,
        "parameters": {
            "scenarios": scenarios,
            "modes": modes,
            "iterations": max(args.iterations, 1),
            "profile_scenarios": profile_scenarios,
            "profile_iterations": max(args.profile_iterations, 1),
            "skip_perf": args.skip_perf,
            "skip_profile": args.skip_profile,
            "stack_profile": stack_profile,
        },
        "perf_results": perf_results,
        "profile_results": profile_results,
    }
    budgets = load_budget_file(args.budget_file)
    report["budget_results"] = evaluate_lashlang_budgets(report, budgets)
    out_path.write_text(json.dumps(report, indent=2))
    print(json.dumps({"out": str(out_path), "perf_results": len(perf_results), "profile_results": len(profile_results)}, indent=2))
    if args.enforce_budgets:
        failures = [item for item in report["budget_results"] if not item.get("passed")]
        if failures:
            for item in failures:
                print(
                    "Lashlang perf budget failed: "
                    f"{item.get('section')} {item.get('scenario')} {item.get('mode') or ''} "
                    f"{item.get('metric')} actual={item.get('actual')} budget={item.get('budget')} "
                    f"reason={item.get('reason') or 'budget exceeded'}",
                    file=sys.stderr,
                )
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
