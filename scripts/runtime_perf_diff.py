#!/usr/bin/env python3
"""Render a before/after table from two lash-perf runtime report ledgers.

A performance change is only worth landing if the same command, on the same
base, moves a number. This tool turns two ``runtime-perf.json`` ledgers into
the table that claim goes in, so the comparison is mechanical instead of
hand-copied.

Three metric families are compared per scenario:

* **duration** — the p50/p95 populations
  ``scripts/runtime_perf_percentiles.py`` reconstructs (``total_wall_ms``,
  ``steady_state_turn_wall_ms``, ``phase:<name>.duration_ms``). Wall clock is
  load-sensitive; read it only from a quiet box.
* **allocation** — ``results[].stages.<stage>.allocations.<field>``, summarized
  as the median across measured runs. Stages absent from a run contribute
  nothing — the run never reached them.
* **counter** — ``results[].extra_counters.<name>``, summarized as the median
  across measured runs. Counters (hash passes, SQL statements, store calls,
  committed bytes) are deterministic and load-independent, so they are the
  honest verdict on a structural change even on a busy box.

Rows are dropped when both sides are zero, and ``--min-delta-pct`` hides
movement below a threshold so a real change is not buried in noise.
``--fail-on-regression`` exits non-zero when a metric worsens by more than the
given percentage, which is what a guard invocation wants.
"""

from __future__ import annotations

import argparse
import json
import math
import statistics
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any, Iterable

sys.path.insert(0, str(Path(__file__).resolve().parent))

from runtime_perf_percentiles import (  # noqa: E402
    RuntimePerfReportError,
    _number,
    _objects,
    round3,
    summarize_report,
)


ALLOCATION_FIELDS = (
    "allocations",
    "bytes_allocated",
)

FAMILY_ORDER = {"duration": 0, "allocation": 1, "counter": 2}


def _median(values: list[float]) -> float:
    return round3(statistics.median(values))


def duration_metrics(report_path: Path) -> dict[tuple[str, str], float]:
    """p50/p95 per scenario and duration metric, keyed ``(scenario, metric)``."""
    metrics: dict[tuple[str, str], float] = {}
    for row in summarize_report(report_path):
        for percentile in ("p50", "p95"):
            metrics[(row["scenario"], f'{row["metric"]}.{percentile}')] = row[percentile]
    return metrics


def _results_by_scenario(report_path: Path) -> dict[str, list[dict[str, Any]]]:
    try:
        payload = json.loads(report_path.read_text())
    except OSError as error:
        raise RuntimePerfReportError(f"cannot read {report_path}: {error}") from error
    except json.JSONDecodeError as error:
        raise RuntimePerfReportError(f"invalid JSON in {report_path}: {error}") from error
    if not isinstance(payload, dict):
        raise RuntimePerfReportError(f"{report_path}: report root must be an object")

    by_scenario: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for index, result in enumerate(_objects(payload.get("results"), f"{report_path}: results")):
        scenario = result.get("scenario")
        if not isinstance(scenario, str) or not scenario:
            raise RuntimePerfReportError(
                f"{report_path}: results[{index}].scenario must be a non-empty string"
            )
        by_scenario[scenario].append(result)
    return by_scenario


def allocation_metrics(report_path: Path) -> dict[tuple[str, str], float]:
    """Median allocation counts and bytes per scenario and measured stage."""
    samples: dict[tuple[str, str], list[float]] = defaultdict(list)
    for scenario, results in _results_by_scenario(report_path).items():
        for index, result in enumerate(results):
            prefix = f"{report_path}: {scenario} result[{index}].stages"
            stages = result.get("stages")
            if not isinstance(stages, dict):
                raise RuntimePerfReportError(f"{prefix} must be an object")
            for stage_name, stage in stages.items():
                if not isinstance(stage, dict):
                    raise RuntimePerfReportError(f"{prefix}.{stage_name} must be an object")
                delta = stage.get("allocations")
                if not isinstance(delta, dict):
                    raise RuntimePerfReportError(
                        f"{prefix}.{stage_name}.allocations must be an object"
                    )
                for field in ALLOCATION_FIELDS:
                    if field not in delta:
                        continue
                    samples[(scenario, f"alloc:{stage_name}.{field}")].append(
                        _number(
                            delta[field], f"{prefix}.{stage_name}.allocations.{field}"
                        )
                    )
    return {key: _median(values) for key, values in samples.items()}


def counter_metrics(report_path: Path) -> dict[tuple[str, str], float]:
    """Median ``extra_counters`` value per scenario and counter name."""
    samples: dict[tuple[str, str], list[float]] = defaultdict(list)
    for scenario, results in _results_by_scenario(report_path).items():
        for index, result in enumerate(results):
            prefix = f"{report_path}: {scenario} result[{index}].extra_counters"
            counters = result.get("extra_counters")
            if counters is None:
                continue
            if not isinstance(counters, dict):
                raise RuntimePerfReportError(f"{prefix} must be an object")
            for name, value in counters.items():
                samples[(scenario, f"counter:{name}")].append(
                    _number(value, f"{prefix}.{name}")
                )
    return {key: _median(values) for key, values in samples.items()}


def _family(metric: str) -> str:
    if metric.startswith("alloc:"):
        return "allocation"
    if metric.startswith("counter:"):
        return "counter"
    return "duration"


def collect(report_path: Path, families: Iterable[str]) -> dict[tuple[str, str], float]:
    selected = set(families)
    metrics: dict[tuple[str, str], float] = {}
    if "duration" in selected:
        metrics.update(duration_metrics(report_path))
    if "allocation" in selected:
        metrics.update(allocation_metrics(report_path))
    if "counter" in selected:
        metrics.update(counter_metrics(report_path))
    return metrics


def delta_percent(before: float | None, after: float | None) -> float | None:
    if before is None or after is None:
        return None
    if before == 0.0:
        return None if after == 0.0 else math.inf
    return (after - before) / abs(before) * 100.0


def build_rows(
    before: dict[tuple[str, str], float],
    after: dict[tuple[str, str], float],
    *,
    scenarios: list[str] | None,
    metric_substrings: list[str],
    min_delta_pct: float,
) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for key in sorted(set(before) | set(after)):
        scenario, metric = key
        if scenarios and scenario not in scenarios:
            continue
        if metric_substrings and not any(part in metric for part in metric_substrings):
            continue
        before_value = before.get(key)
        after_value = after.get(key)
        if (before_value or 0.0) == 0.0 and (after_value or 0.0) == 0.0:
            continue
        percent = delta_percent(before_value, after_value)
        if (
            percent is not None
            and math.isfinite(percent)
            and abs(percent) < min_delta_pct
        ):
            continue
        rows.append(
            {
                "scenario": scenario,
                "metric": metric,
                "family": _family(metric),
                "before": before_value,
                "after": after_value,
                "delta": None
                if before_value is None or after_value is None
                else round3(after_value - before_value),
                "delta_pct": percent,
            }
        )
    rows.sort(key=lambda row: (row["scenario"], FAMILY_ORDER[row["family"]], row["metric"]))
    return rows


def _value_cell(value: float | None) -> str:
    if value is None:
        return "not measured"
    if value == int(value) and abs(value) < 1e15:
        return f"{int(value):,}"
    return f"{value:,.3f}"


def _percent_cell(percent: float | None) -> str:
    if percent is None:
        return "—"
    if math.isinf(percent):
        return "new"
    return f"{percent:+.1f}%"


def render_table(rows: list[dict[str, Any]]) -> str:
    lines = [
        "| scenario | metric | before | after | delta | delta % |",
        "|---|---|---:|---:|---:|---:|",
    ]
    for row in rows:
        lines.append(
            "| "
            + " | ".join(
                [
                    str(row["scenario"]).replace("|", "\\|"),
                    str(row["metric"]).replace("|", "\\|"),
                    _value_cell(row["before"]),
                    _value_cell(row["after"]),
                    _value_cell(row["delta"]),
                    _percent_cell(row["delta_pct"]),
                ]
            )
            + " |"
        )
    return "\n".join(lines)


def regressions(rows: list[dict[str, Any]], threshold_pct: float) -> list[dict[str, Any]]:
    return [
        row
        for row in rows
        if row["delta_pct"] is not None
        and math.isfinite(row["delta_pct"])
        and row["delta_pct"] > threshold_pct
    ]


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("before", type=Path, help="the baseline runtime report JSON")
    parser.add_argument("after", type=Path, help="the candidate runtime report JSON")
    parser.add_argument(
        "--scenario",
        action="append",
        default=[],
        help="limit the table to this scenario; repeatable",
    )
    parser.add_argument(
        "--metric",
        action="append",
        default=[],
        help="keep only metrics containing this substring; repeatable",
    )
    parser.add_argument(
        "--family",
        action="append",
        choices=sorted(FAMILY_ORDER),
        default=[],
        help="limit to a metric family (default: all three); repeatable",
    )
    parser.add_argument(
        "--min-delta-pct",
        type=float,
        default=0.0,
        help="hide rows whose magnitude of change is below this percentage",
    )
    parser.add_argument(
        "--fail-on-regression",
        type=float,
        metavar="PCT",
        help="exit 1 when any kept metric grows by more than PCT percent",
    )
    parser.add_argument("--json", action="store_true", help="emit rows as JSON instead of Markdown")
    parser.add_argument("--out", type=Path, help="also write the rendered output to this path")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    families = args.family or sorted(FAMILY_ORDER)
    try:
        before = collect(args.before, families)
        after = collect(args.after, families)
    except RuntimePerfReportError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    rows = build_rows(
        before,
        after,
        scenarios=args.scenario or None,
        metric_substrings=args.metric,
        min_delta_pct=args.min_delta_pct,
    )
    rendered = json.dumps(rows, indent=2) if args.json else render_table(rows)
    print(rendered)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(rendered + "\n")

    if args.fail_on_regression is not None:
        regressed = regressions(rows, args.fail_on_regression)
        if regressed:
            for row in regressed:
                print(
                    f"regression: {row['scenario']} {row['metric']} "
                    f"{row['before']} -> {row['after']} ({row['delta_pct']:+.1f}%)",
                    file=sys.stderr,
                )
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
