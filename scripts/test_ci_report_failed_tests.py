#!/usr/bin/env python3
"""Tests for Buck2 failed-test report staging."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "report_failed_tests", ROOT / "scripts/ci/report_failed_tests.py"
)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class FailedTestReportTests(unittest.TestCase):
    def test_selects_only_failed_results(self) -> None:
        report = {
            "results": {
                "root//pkg:pass": {"status": "PASS"},
                "root//pkg:fail": {"status": "FAIL", "exit_code": 1},
            }
        }
        self.assertEqual(MODULE.failed_targets(report), [("//pkg:fail", report["results"]["root//pkg:fail"])])

    def test_stages_declared_xml_log_and_undeclared_outputs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            xml = root / "source.xml"
            log = root / "source.log"
            undeclared = root / "source-undeclared"
            xml.write_text('<testsuite><testcase name="bad"><failure/></testcase></testsuite>')
            log.write_text("failure\n")
            undeclared.mkdir()
            (undeclared / "receipt.json").write_text(json.dumps({"ok": False}))
            destination = MODULE.stage_result(
                "//pkg:target",
                {"outputs": {"junit_xml": str(xml), "log": str(log), "undeclared": str(undeclared)}},
                root / "stage",
            )
            self.assertEqual((destination / "test.log").read_text(), "failure\n")
            self.assertTrue((destination / "test.xml").is_file())
            self.assertTrue((destination / "undeclared/receipt.json").is_file())
            self.assertEqual(MODULE.failing_cases(destination / "test.xml"), ["::bad"])


if __name__ == "__main__":
    unittest.main()
