#!/usr/bin/env python3
from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import perfreport


ROOT = Path(__file__).resolve().parents[1]
EXPECTED_KINDS = {
    "dhat",
    "lashlang-perf",
    "perf-guard",
    "runtime-guard",
    "runtime-perf",
    "runtime-stack",
    "ui-perf",
}


class PerfReportDispatchTest(unittest.TestCase):
    def test_dispatch_table_has_the_complete_exact_kind_set(self) -> None:
        self.assertEqual(set(perfreport.REPORT_DISPATCH), EXPECTED_KINDS)

    def test_dispatch_ignores_report_shape_and_uses_kind(self) -> None:
        kind, _summarize, _diff = perfreport.dispatch_entry(
            {"kind": "runtime-perf", "summary": []}, Path("runtime.json")
        )
        self.assertEqual(kind, "runtime-perf")

    def test_missing_and_unknown_kinds_name_the_observed_tag(self) -> None:
        for payload, observed in (({}, "'<missing>'"), ({"kind": "future"}, "'future'")):
            with self.subTest(payload=payload):
                with self.assertRaisesRegex(ValueError, rf"unknown report kind {observed}"):
                    perfreport.dispatch_entry(payload, Path("report.json"))

    def test_cli_reports_missing_kind_loudly(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            report = Path(tmp) / "report.json"
            report.write_text(json.dumps({"summary": []}))
            completed = subprocess.run(
                [sys.executable, str(ROOT / "scripts" / "perfreport.py"), str(report)],
                check=False,
                capture_output=True,
                text=True,
            )

        self.assertEqual(completed.returncode, 2)
        self.assertIn("unknown report kind '<missing>'", completed.stderr)


if __name__ == "__main__":
    unittest.main()
