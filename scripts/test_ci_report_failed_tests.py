#!/usr/bin/env python3
"""Behavior tests for scripts/ci/report_failed_tests.py."""

from __future__ import annotations

import importlib.util
import pathlib
import sys
import tempfile
import unittest
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location(
    "report_failed_tests", ROOT / "scripts" / "ci" / "report_failed_tests.py"
)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


JUNIT_FAILED = """<?xml version="1.0"?>
<testsuite tests="3" failures="1" errors="0">
  <testcase classname="tests::a" name="one"/>
  <testcase classname="tests::a" name="two"><failure>assert failed</failure></testcase>
  <testcase classname="tests::b" name="three"/>
</testsuite>
"""

JUNIT_PASSED = """<?xml version="1.0"?>
<testsuite tests="1"><testcase classname="tests::a" name="one"/></testsuite>
"""


class FailedTargetParsingTests(unittest.TestCase):
    def test_verdict_lines_and_kill_banner_are_named(self) -> None:
        output = "\n".join(
            [
                "(04:49:01) FAIL: //crates/lash:lash__unit_test (Killed) (see /tmp/x/test.log)",
                "\x1b[36;1m//crates/lash:lash__unit_test FAILED in 62.7s\x1b[0m",
                "//pkg/a:one (shard 2 of 4) FAILED in 5.0s",
                "//pkg/a:two TIMEOUT in 300.0s",
                "//pkg/a:three PASSED in 1.0s",
                "//pkg/a:one FLAKY, failed in 3.0s but passed on retry",
                "Executed 4 out of 4 tests: 2 fail.",
            ]
        )
        rows = MODULE.failed_targets(output)
        self.assertEqual(
            ["//crates/lash:lash__unit_test", "//pkg/a:one", "//pkg/a:two"],
            [row["label"] for row in rows],
        )
        self.assertTrue(rows[0]["killed"])
        self.assertEqual("FAILED", rows[1]["verdict"])
        self.assertEqual("TIMEOUT", rows[2]["verdict"])

    def test_testlogs_dir_maps_the_label(self) -> None:
        self.assertEqual(
            pathlib.Path("tl/crates/lash/lash__unit_test"),
            MODULE.testlogs_dir(pathlib.Path("tl"), "//crates/lash:lash__unit_test"),
        )
        self.assertEqual(
            pathlib.Path("tl/pkg/a/b"),
            MODULE.testlogs_dir(pathlib.Path("tl"), "@@//pkg/a:b"),
        )


class ReportFlowTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        root = pathlib.Path(self.temp.name)
        self.output = root / "bazel-test-output.txt"
        self.testlogs = root / "bazel-testlogs"
        self.stage = root / "staging"

    def tearDown(self) -> None:
        self.temp.cleanup()

    def write_logs(self, label_dir: str, *, log: bool = True, xml: str | None = None) -> None:
        directory = self.testlogs / label_dir
        directory.mkdir(parents=True)
        if log:
            (directory / "test.log").write_text("test output\n")
        if xml is not None:
            (directory / "test.xml").write_text(xml)

    def run_report(self) -> None:
        argv = [
            "report_failed_tests.py",
            "--output", str(self.output),
            "--testlogs", str(self.testlogs),
            "--stage", str(self.stage),
        ]
        with mock.patch.object(sys, "argv", argv):
            self.assertEqual(0, MODULE.main())

    def test_failed_target_stages_logs_and_names_cases(self) -> None:
        self.output.write_text("//pkg/a:one FAILED in 5.0s\n")
        self.write_logs("pkg/a/one", xml=JUNIT_FAILED)
        self.write_logs("pkg/a/two", xml=JUNIT_PASSED)
        self.run_report()
        staged = self.stage / "pkg/a/one"
        self.assertTrue((staged / "test.log").is_file())
        self.assertTrue((staged / "test.xml").is_file())
        self.assertFalse((self.stage / "pkg/a/two").exists())
        self.assertEqual(
            ["tests::a::two"],
            MODULE.failing_cases(self.testlogs / "pkg/a/one"),
        )

    def test_killed_target_without_log_is_reported_not_invented(self) -> None:
        self.output.write_text("FAIL: //pkg/a:one (Killed) (see /x/test.log)\n")
        self.write_logs("pkg/a/one", log=False)
        self.run_report()
        staged = self.stage / "pkg/a/one"
        self.assertTrue(staged.is_dir())
        self.assertFalse((staged / "test.log").exists())

    def test_missing_output_file_is_a_clean_report(self) -> None:
        self.run_report()
        self.assertFalse(self.stage.exists())


if __name__ == "__main__":
    unittest.main()
