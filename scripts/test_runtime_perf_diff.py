#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import json
import math
import pathlib
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parent.parent


def load_diff_module():
    module_path = ROOT / "scripts" / "runtime_perf_diff.py"
    spec = importlib.util.spec_from_file_location("runtime_perf_diff", module_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"failed to load {module_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


DIFF = load_diff_module()


def report(
    *,
    total_ms: list[float],
    counters: dict[str, int],
    bytes_allocated: int,
) -> dict:
    return {
        "results": [
            {
                "scenario": "durable_standard_tool_turn_sqlite",
                "total_ms": value,
                "turns": [
                    {"total_ms": value / 2, "phase_profile": {}},
                    {"total_ms": value / 2, "phase_profile": {}},
                ],
                "phase_profile": {"CommittedTurn": {"duration_ms": value / 4}},
                "allocations": {
                    "total": {"allocations": 100, "bytes_allocated": bytes_allocated}
                },
                "extra_counters": dict(counters),
            }
            for value in total_ms
        ]
    }


def write(directory: pathlib.Path, name: str, payload: dict) -> pathlib.Path:
    path = directory / name
    path.write_text(json.dumps(payload))
    return path


class RuntimePerfDiffTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tempdir = tempfile.TemporaryDirectory()
        self.directory = pathlib.Path(self._tempdir.name)
        self.addCleanup(self._tempdir.cleanup)
        self.before = write(
            self.directory,
            "before.json",
            report(
                total_ms=[100.0, 120.0, 140.0],
                counters={"runtime_work.hash_passes": 43, "store_calls.total": 12},
                bytes_allocated=2_000,
            ),
        )
        self.after = write(
            self.directory,
            "after.json",
            report(
                total_ms=[80.0, 90.0, 100.0],
                counters={"runtime_work.hash_passes": 21, "store_calls.total": 12},
                bytes_allocated=1_500,
            ),
        )

    def rows(self, **overrides):
        kwargs = {
            "scenarios": None,
            "metric_substrings": [],
            "min_delta_pct": 0.0,
        }
        kwargs.update(overrides)
        return DIFF.build_rows(
            DIFF.collect(self.before, sorted(DIFF.FAMILY_ORDER)),
            DIFF.collect(self.after, sorted(DIFF.FAMILY_ORDER)),
            **kwargs,
        )

    def row(self, metric: str, rows=None):
        rows = self.rows() if rows is None else rows
        matches = [row for row in rows if row["metric"] == metric]
        self.assertEqual(len(matches), 1, f"expected exactly one {metric} row in {rows}")
        return matches[0]

    def test_counter_delta_is_the_median_across_runs(self) -> None:
        row = self.row("counter:runtime_work.hash_passes")
        self.assertEqual(row["before"], 43)
        self.assertEqual(row["after"], 21)
        self.assertEqual(row["delta"], -22)
        self.assertAlmostEqual(row["delta_pct"], -51.16279, places=4)

    def test_duration_rows_carry_both_percentiles(self) -> None:
        p50 = self.row("total_wall_ms.p50")
        p95 = self.row("total_wall_ms.p95")
        self.assertEqual(p50["before"], 120.0)
        self.assertEqual(p50["after"], 90.0)
        self.assertEqual(p95["before"], 138.0)
        self.assertEqual(p95["after"], 99.0)

    def test_allocation_bytes_are_compared(self) -> None:
        row = self.row("alloc:total.bytes_allocated")
        self.assertEqual(row["before"], 2_000)
        self.assertEqual(row["after"], 1_500)
        self.assertAlmostEqual(row["delta_pct"], -25.0)

    def test_unchanged_metrics_are_hidden_by_a_minimum_delta(self) -> None:
        rows = self.rows(min_delta_pct=1.0)
        self.assertNotIn(
            "counter:store_calls.total", [row["metric"] for row in rows]
        )
        self.assertIn("counter:runtime_work.hash_passes", [row["metric"] for row in rows])

    def test_metric_substring_filter_narrows_the_table(self) -> None:
        rows = self.rows(metric_substrings=["hash_passes"])
        self.assertEqual([row["metric"] for row in rows], ["counter:runtime_work.hash_passes"])

    def test_a_metric_missing_from_one_side_is_reported_not_zeroed(self) -> None:
        after = json.loads(self.after.read_text())
        for result in after["results"]:
            result["extra_counters"]["runtime_work.sql_statements"] = 7
        new_after = write(self.directory, "after-sql.json", after)
        rows = DIFF.build_rows(
            DIFF.collect(self.before, sorted(DIFF.FAMILY_ORDER)),
            DIFF.collect(new_after, sorted(DIFF.FAMILY_ORDER)),
            scenarios=None,
            metric_substrings=["sql_statements"],
            min_delta_pct=0.0,
        )
        row = self.row("counter:runtime_work.sql_statements", rows)
        self.assertIsNone(row["before"])
        self.assertEqual(row["after"], 7)
        self.assertIsNone(row["delta"])
        self.assertEqual(DIFF._value_cell(row["before"]), "not measured")

    def test_regression_gate_fires_only_above_the_threshold(self) -> None:
        rows = DIFF.build_rows(
            DIFF.collect(self.after, sorted(DIFF.FAMILY_ORDER)),
            DIFF.collect(self.before, sorted(DIFF.FAMILY_ORDER)),
            scenarios=None,
            metric_substrings=["hash_passes"],
            min_delta_pct=0.0,
        )
        self.assertEqual(len(DIFF.regressions(rows, 200.0)), 0)
        self.assertEqual(len(DIFF.regressions(rows, 5.0)), 1)

    def test_new_metric_renders_as_new_not_as_infinity(self) -> None:
        self.assertEqual(DIFF._percent_cell(math.inf), "new")
        self.assertEqual(DIFF._percent_cell(None), "—")

    def test_cli_renders_a_markdown_table_and_honours_the_regression_gate(self) -> None:
        completed = subprocess.run(
            [
                sys.executable,
                str(ROOT / "scripts" / "runtime_perf_diff.py"),
                str(self.before),
                str(self.after),
                "--metric",
                "hash_passes",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("| scenario | metric | before | after | delta | delta % |", completed.stdout)
        self.assertIn("runtime_work.hash_passes", completed.stdout)

        regressed = subprocess.run(
            [
                sys.executable,
                str(ROOT / "scripts" / "runtime_perf_diff.py"),
                str(self.after),
                str(self.before),
                "--metric",
                "hash_passes",
                "--fail-on-regression",
                "5",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(regressed.returncode, 1, regressed.stdout)
        self.assertIn("regression:", regressed.stderr)


if __name__ == "__main__":
    unittest.main()
