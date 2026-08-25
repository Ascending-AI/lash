#!/usr/bin/env python3
"""Reconstruct runtime-perf p50/p95/p99 summaries from serialized samples.

For each scenario, the table contains the same duration populations as the
Rust runtime report summary:

* ``total_wall_ms`` uses one ``results[].total_ms`` sample per measured run.
* ``steady_state_turn_wall_ms`` first averages ``turns[1:].total_ms`` within
  each measured run, then uses one such steady-state sample per run.
* ``phase:<name>.duration_ms`` uses one named ``results[].phase_profile``
  duration total per measured run.

Percentiles use linear interpolation over sorted samples at the zero-based
rank ``p * (n - 1)``. A fractional rank is interpolated between its two
neighbors. One sample returns that sample; two samples interpolate directly
between their endpoints. This is the same definition as
``crates/lash-perf/src/perf_support/metrics.rs``.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any, Iterable


PERCENTILES = (0.50, 0.95, 0.99)


class RuntimePerfReportError(ValueError):
    """A report does not contain the runtime-perf sample contract."""


def round3(value: float) -> float:
    """Match Rust's ``f64::round`` followed by division by 1,000."""
    scaled = value * 1_000.0
    rounded = math.floor(scaled + 0.5) if scaled >= 0 else math.ceil(scaled - 0.5)
    return rounded / 1_000.0


def percentile_sorted(values: list[float], percentile: float) -> float:
    """Return the Rust runtime-report percentile for already-sorted samples."""
    if not values:
        return 0.0
    if len(values) == 1:
        return values[0]
    rank = min(max(percentile, 0.0), 1.0) * (len(values) - 1)
    lower = math.floor(rank)
    upper = math.ceil(rank)
    if lower == upper:
        return values[lower]
    weight = rank - lower
    return values[lower] * (1.0 - weight) + values[upper] * weight


def percentile_summary(values: Iterable[float]) -> dict[str, float]:
    ordered = sorted(values)
    return {
        f"p{int(percentile * 100)}": round3(percentile_sorted(ordered, percentile))
        for percentile in PERCENTILES
    }


def _number(value: Any, location: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise RuntimePerfReportError(f"{location} must be a number")
    number = float(value)
    if not math.isfinite(number):
        raise RuntimePerfReportError(f"{location} must be finite")
    return number


def _objects(value: Any, location: str) -> list[dict[str, Any]]:
    if not isinstance(value, list):
        raise RuntimePerfReportError(f"{location} must be an array")
    objects = []
    for index, item in enumerate(value):
        if not isinstance(item, dict):
            raise RuntimePerfReportError(f"{location}[{index}] must be an object")
        objects.append(item)
    return objects


def _row(report: Path, scenario: str, metric: str, values: list[float]) -> dict[str, Any]:
    summary = percentile_summary(values)
    return {
        "report": str(report),
        "scenario": scenario,
        "metric": metric,
        "samples": len(values),
        **summary,
    }


def summarize_report(report_path: Path) -> list[dict[str, Any]]:
    try:
        payload = json.loads(report_path.read_text())
    except OSError as error:
        raise RuntimePerfReportError(f"cannot read {report_path}: {error}") from error
    except json.JSONDecodeError as error:
        raise RuntimePerfReportError(f"invalid JSON in {report_path}: {error}") from error
    if not isinstance(payload, dict):
        raise RuntimePerfReportError(f"{report_path}: report root must be an object")

    results = _objects(payload.get("results"), f"{report_path}: results")
    by_scenario: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for index, result in enumerate(results):
        scenario = result.get("scenario")
        if not isinstance(scenario, str) or not scenario:
            raise RuntimePerfReportError(
                f"{report_path}: results[{index}].scenario must be a non-empty string"
            )
        by_scenario[scenario].append(result)

    rows: list[dict[str, Any]] = []
    for scenario, scenario_results in sorted(by_scenario.items()):
        totals: list[float] = []
        steady_turn_totals: list[float] = []
        phase_totals: dict[str, list[float]] = defaultdict(list)

        for result_index, result in enumerate(scenario_results):
            prefix = f"{report_path}: {scenario} result[{result_index}]"
            totals.append(_number(result.get("total_ms"), f"{prefix}.total_ms"))

            turns = _objects(result.get("turns"), f"{prefix}.turns")
            steady_turns = turns[1:]
            if steady_turns:
                steady_total = sum(
                    _number(turn.get("total_ms"), f"{prefix}.turns[{index + 1}].total_ms")
                    for index, turn in enumerate(steady_turns)
                ) / len(steady_turns)
                # Rust rounds each run's steady-state mean before summarizing runs.
                steady_turn_totals.append(round3(steady_total))

            phase_profile = result.get("phase_profile")
            if not isinstance(phase_profile, dict):
                raise RuntimePerfReportError(f"{prefix}.phase_profile must be an object")
            for phase, metrics in phase_profile.items():
                if not isinstance(phase, str) or not phase:
                    raise RuntimePerfReportError(
                        f"{prefix}.phase_profile keys must be non-empty strings"
                    )
                if not isinstance(metrics, dict):
                    raise RuntimePerfReportError(
                        f"{prefix}.phase_profile[{phase!r}] must be an object"
                    )
                phase_totals[phase].append(
                    _number(
                        metrics.get("duration_ms"),
                        f"{prefix}.phase_profile[{phase!r}].duration_ms",
                    )
                )

        rows.append(_row(report_path, scenario, "total_wall_ms", totals))
        if steady_turn_totals:
            rows.append(
                _row(
                    report_path,
                    scenario,
                    "steady_state_turn_wall_ms",
                    steady_turn_totals,
                )
            )
        rows.extend(
            _row(report_path, scenario, f"phase:{phase}.duration_ms", values)
            for phase, values in sorted(phase_totals.items())
        )
    return rows


def _cell(value: Any) -> str:
    return str(value).replace("|", "\\|")


def render_table(rows: list[dict[str, Any]]) -> str:
    lines = [
        "| report | scenario | metric | samples | p50_ms | p95_ms | p99_ms |",
        "|---|---|---|---:|---:|---:|---:|",
    ]
    for row in rows:
        lines.append(
            "| "
            + " | ".join(
                [
                    _cell(row["report"]),
                    _cell(row["scenario"]),
                    _cell(row["metric"]),
                    str(row["samples"]),
                    f'{row["p50"]:.3f}',
                    f'{row["p95"]:.3f}',
                    f'{row["p99"]:.3f}',
                ]
            )
            + " |"
        )
    return "\n".join(lines)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "reports",
        nargs="+",
        type=Path,
        help="one or more lash-perf runtime report JSON paths",
    )
    parser.add_argument("--out", type=Path, help="also write the Markdown table to this path")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        rows = [row for report in args.reports for row in summarize_report(report)]
    except RuntimePerfReportError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    table = render_table(rows)
    print(table)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(table + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
