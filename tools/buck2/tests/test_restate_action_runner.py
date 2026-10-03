import contextlib
import importlib.util
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[3]
SPEC = importlib.util.spec_from_file_location("restate_action_runner", ROOT / "tools/buck2/restate_action_runner.py")
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


class SelectionTests(unittest.TestCase):
    def test_shards_partition_the_registered_union_without_repeating_laws(self):
        names = [f"tests::live_{index}" for index in range(31)]
        shards = [RUNNER.select(names, ["--ignored"], 4, index) for index in range(4)]
        self.assertEqual(sorted(names), sorted(name for shard in shards for name in shard))
        self.assertTrue(all(shards))
        self.assertEqual(shards, [RUNNER.select(names[::-1], [], 4, index)[::-1] for index in range(4)])

    def test_exact_and_skip_select_only_registered_laws(self):
        names = ["tests::live_a", "tests::live_ab", "tests::other"]
        self.assertEqual([names[0]], RUNNER.select(names, ["--ignored", "--exact", names[0]], 1, 0))
        self.assertEqual([names[0]], RUNNER.select(names, ["live_", "--skip", names[1]], 1, 0))
        self.assertEqual([names[1]], RUNNER.select(names, ["--skip=" + names[0], "--exact", names[1]], 1, 0))
        with self.assertRaisesRegex(ValueError, "no registered law"):
            RUNNER.select(names, ["not-in-this-suite"], 1, 0)

    def test_an_unsupported_selector_is_refused(self):
        with self.assertRaisesRegex(ValueError, "unsupported"):
            RUNNER.select(["tests::live"], ["--list"], 1, 0)


class ReportTests(unittest.TestCase):
    def test_report_names_executed_laws_and_retains_strict_replay_divergences(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            cases = [dict(name="tests::passes", status="ok", seconds=.1),
                     dict(name="tests::held", status="failed", seconds=.2),
                     dict(name="tests::healed", status="ok", seconds=.3)]
            for case in cases:
                (root / (case["name"].replace("::", "__") + ".log")).write_text("test result: ok. 1 passed; 0 failed")
            with mock.patch.dict(os.environ, TEST_TARGET="//fake:suite", XML_OUTPUT_FILE=str(root / "test.xml")), \
                 contextlib.redirect_stdout(io.StringIO()) as output:
                RUNNER.write_report(dict(tests=cases, held=["tests::held", "tests::healed"], healed=["tests::healed"]), root, 1)
            report = ET.parse(root / "test.xml").getroot()
            self.assertEqual(report.attrib["tests"], "3")
            self.assertEqual(report.attrib["failures"], "1")
            self.assertEqual(report.attrib["skipped"], "1")
            self.assertEqual([node.attrib["name"] for node in report.findall("testcase")], [case["name"] for case in cases])
            self.assertIn("test tests::healed ... FAILED", output.getvalue())
            self.assertEqual(len(report.findall("testcase/system-out")), 3)

    def test_a_missing_law_records_a_suite_error_even_when_the_completed_law_passes(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            (root / "tests__passes.log").write_text("test result: ok. 1 passed; 0 failed")
            with mock.patch.dict(os.environ, TEST_TARGET="//fake:suite", XML_OUTPUT_FILE=str(root / "test.xml")), \
                 contextlib.redirect_stdout(io.StringIO()):
                RUNNER.write_report(dict(tests=[dict(name="tests::passes", status="ok", seconds=.1)], held=[], healed=[]), root, 1)
            self.assertEqual(ET.parse(root / "test.xml").getroot().attrib["errors"], "1")


if __name__ == "__main__":
    unittest.main()
