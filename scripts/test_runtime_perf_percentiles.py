#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parent.parent


def load_percentiles_module():
    module_path = ROOT / "scripts" / "runtime_perf_percentiles.py"
    spec = importlib.util.spec_from_file_location("runtime_perf_percentiles", module_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"failed to load {module_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run_result(total_ms: float, turns: list[float], phase_ms: dict[str, float]) -> dict:
    return {
        "scenario": "standard",
        "total_ms": total_ms,
        "turns": [{"total_ms": value, "phase_profile": {}} for value in turns],
        "phase_profile": {
            phase: {"duration_ms": duration} for phase, duration in phase_ms.items()
        },
    }


class RuntimePerfPercentilesTest(unittest.TestCase):
    def test_round3_uses_half_away_from_zero(self) -> None:
        percentiles = load_percentiles_module()

        self.assertEqual(percentiles.round3(1.5025), 1.503)

    def test_linear_percentiles_cover_one_and_two_samples(self) -> None:
        percentiles = load_percentiles_module()

        self.assertEqual(
            percentiles.percentile_summary([7.25]),
            {"p50": 7.25, "p95": 7.25, "p99": 7.25},
        )
        self.assertEqual(
            percentiles.percentile_summary([10.0, 20.0]),
            {"p50": 15.0, "p95": 19.5, "p99": 19.9},
        )

    def test_report_rows_match_runtime_summary_populations(self) -> None:
        percentiles = load_percentiles_module()
        payload = {
            "results": [
                run_result(10.0, [100.0, 2.0, 4.0], {"commit": 10.0, "wake": 2.0}),
                run_result(20.0, [100.0, 6.0, 10.0], {"commit": 20.0, "wake": 4.0}),
                run_result(30.0, [100.0, 8.0, 12.0], {"commit": 30.0, "wake": 8.0}),
            ]
        }

        with tempfile.TemporaryDirectory() as tmp:
            report = pathlib.Path(tmp) / "runtime.json"
            report.write_text(json.dumps(payload))
            rows = {
                row["metric"]: row for row in percentiles.summarize_report(report)
            }

        self.assertEqual(rows["total_wall_ms"]["samples"], 3)
        self.assertEqual(
            {key: rows["total_wall_ms"][key] for key in ("p50", "p95", "p99")},
            {"p50": 20.0, "p95": 29.0, "p99": 29.8},
        )
        # The first turn is warm; steady samples are per-run means 3, 8, and 10.
        self.assertEqual(
            {
                key: rows["steady_state_turn_wall_ms"][key]
                for key in ("p50", "p95", "p99")
            },
            {"p50": 8.0, "p95": 9.8, "p99": 9.96},
        )
        self.assertEqual(rows["phase:commit.duration_ms"]["p95"], 29.0)
        self.assertEqual(rows["phase:wake.duration_ms"]["p99"], 7.92)

    def test_cli_prints_and_writes_the_same_table(self) -> None:
        payload = {"results": [run_result(12.0, [20.0, 5.0], {"commit": 3.0})]}
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            report = root / "runtime.json"
            output = root / "percentiles.md"
            report.write_text(json.dumps(payload))

            completed = subprocess.run(
                [
                    sys.executable,
                    str(ROOT / "scripts" / "runtime_perf_percentiles.py"),
                    str(report),
                    "--out",
                    str(output),
                ],
                check=True,
                capture_output=True,
                text=True,
            )

            self.assertEqual(completed.stdout, output.read_text())
            self.assertIn("| total_wall_ms | 1 | 12.000 | 12.000 | 12.000 |", completed.stdout)
            self.assertIn(
                "| steady_state_turn_wall_ms | 1 | 5.000 | 5.000 | 5.000 |",
                completed.stdout,
            )


if __name__ == "__main__":
    unittest.main()
