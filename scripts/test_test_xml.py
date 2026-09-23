#!/usr/bin/env python3
"""The self-written JUnit report: `junit_xml.py` and the `test_xml_runner.sh` prefix."""

import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import unittest
import xml.etree.ElementTree as ET


TOOLS = Path(__file__).resolve().parents[1] / "tools/bazel"
WRITER = TOOLS / "junit_xml.py"
RUNNER = TOOLS / "test_xml_runner.sh"

LIBTEST_LOG = b"""running 3 tests
test a::passes ... ok
test a::fails ... FAILED
test a::slow ... ignored, needs a service
not xml \x01 and not utf-8 \xff

failures:

---- a::fails stdout ----
thread 'a::fails' panicked at src/lib.rs:3:5:
left <&> right

failures:
    a::fails

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out
"""


def cases(suite):
    return {case.get("name"): case for case in suite.iter("testcase")}


class WriterTests(unittest.TestCase):
    def write(self, *suites):
        with tempfile.TemporaryDirectory() as tmp:
            argv = []
            for index, (name, code, log) in enumerate(suites):
                path = Path(tmp, f"{index}.log")
                path.write_bytes(log)
                argv += [name, code, "1.500", str(path)]
            out = Path(tmp, "test.xml")
            subprocess.run(["python3", str(WRITER), str(out), *argv], check=True)
            return ET.parse(out).getroot()

    def test_libtest_lines_become_cases_with_failure_detail(self):
        (suite,) = self.write(("crates/x/x__test", "101", LIBTEST_LOG))
        self.assertEqual(
            (suite.get("name"), suite.get("tests"), suite.get("failures"),
             suite.get("errors"), suite.get("skipped"), suite.get("time")),
            ("crates/x/x__test", "3", "1", "0", "1", "1.500"),
        )
        found = cases(suite)
        self.assertEqual(set(found), {"a::passes", "a::fails", "a::slow"})
        self.assertEqual(len(found["a::passes"]), 0)
        self.assertIsNotNone(found["a::slow"].find("skipped"))
        failure = found["a::fails"].find("failure").text
        self.assertIn("panicked at src/lib.rs:3:5", failure)
        self.assertIn("left <&> right", failure)
        self.assertIn("not xml ? and not utf-8 �", suite.find("system-out").text)

    def test_exit_without_a_failed_case_is_an_error_case(self):
        (suite,) = self.write(("crates/x/x__test", "134", b"test a::ok ... ok\nabort\n"))
        self.assertEqual((suite.get("tests"), suite.get("errors")), ("2", "1"))
        error = cases(suite)["crates/x/x__test"].find("error")
        self.assertEqual(error.get("message"), "exited with error code 134")

    def test_non_libtest_output_is_one_case_per_binary(self):
        passed, missing = self.write(("ui", "0", b"all fixtures match\n"), ("gone", "?", b""))
        self.assertEqual(list(cases(passed)), ["ui"])
        self.assertEqual((passed.get("failures"), passed.get("errors")), ("0", "0"))
        self.assertEqual(
            cases(missing)["gone"].find("error").get("message"),
            "exited without recording an exit code",
        )


class RunnerTests(unittest.TestCase):
    def run_under(self, script, *, env=(), stdin="", terminate_after=None):
        """Runs the prefix in its own process group, as test-setup.sh does,
        and returns once the prefix exits -- not when every writer of its
        output is gone -- then kills the group, as test-setup.sh does."""
        with tempfile.TemporaryDirectory() as tmp:
            xml = Path(tmp, "test.xml")
            out = Path(tmp, "stdout")
            with out.open("w") as stdout:
                proc = subprocess.Popen(
                    ["bash", str(RUNNER), "bash", "-c", script],
                    stdin=subprocess.PIPE, stdout=stdout, stderr=subprocess.STDOUT,
                    text=True, start_new_session=True,
                    env=dict(os.environ, XML_OUTPUT_FILE=str(xml),
                             TEST_BINARY="crates/x/x__test", TEST_TMPDIR=tmp, **dict(env)),
                )
                try:
                    if terminate_after is not None:
                        time.sleep(terminate_after)
                        os.killpg(proc.pid, signal.SIGTERM)
                    proc.communicate(stdin, timeout=10)
                finally:
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            return proc.returncode, out.read_text(), (ET.parse(xml).getroot() if xml.exists() else None)

    def test_exit_code_output_and_stdin_pass_through_and_report_is_written(self):
        code, output, report = self.run_under(
            'read -r line; echo "got $line"; echo "test a::b ... FAILED"; echo err >&2; exit 7',
            stdin="input\n",
        )
        self.assertEqual(code, 7)
        self.assertEqual(output, "got input\ntest a::b ... FAILED\nerr\n")
        (suite,) = report
        self.assertEqual(suite.get("name"), "crates/x/x__test")
        self.assertEqual(list(cases(suite)), ["a::b"])
        self.assertEqual(suite.find("system-out").text, output)

    def test_shards_get_distinct_suite_names(self):
        _, _, report = self.run_under("true", env={"TEST_TOTAL_SHARDS": "8", "TEST_SHARD_INDEX": "2"})
        self.assertEqual(report[0].get("name"), "crates/x/x__test_shard_3/8")

    def test_a_report_the_test_wrote_is_kept(self):
        code, _, report = self.run_under(
            'echo "<testsuites><testsuite name=\\"own\\"/></testsuites>" > "$XML_OUTPUT_FILE"'
        )
        self.assertEqual(code, 0)
        self.assertEqual(report[0].get("name"), "own")

    def test_a_left_behind_writer_does_not_hold_the_test(self):
        code, _, report = self.run_under("echo done; (sleep 20; echo late) &")
        self.assertEqual(code, 0)
        self.assertEqual(report[0].find("system-out").text, "done\n")

    def test_a_timeout_signal_still_leaves_a_report(self):
        # test-setup.sh forwards a timeout's SIGTERM to the whole group.
        code, _, report = self.run_under("echo started; sleep 20", terminate_after=0.5)
        self.assertEqual(code, 128 + signal.SIGTERM)
        (suite,) = report
        self.assertEqual(suite.find("system-out").text, "started\n")
        self.assertEqual(
            cases(suite)["crates/x/x__test"].find("error").get("message"),
            "exited with error code 143",
        )


if __name__ == "__main__":
    unittest.main()
