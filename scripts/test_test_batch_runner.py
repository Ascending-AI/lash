#!/usr/bin/env python3
"""Exercise batch argument and result reporting with cheap libtest-shaped members."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import xml.etree.ElementTree as ET


RUNNER = Path(__file__).resolve().parents[1] / "tools/buck2/test_batch_runner.sh"


class BatchRunnerTests(unittest.TestCase):
    def invoke(self, *args, failing=False, jobs="2"):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "_main").mkdir()
            logs = root / "logs"
            logs.mkdir()
            for name in ("first", "second"):
                member = root / "_main" / name
                member.write_text(
                    '#!/usr/bin/env python3\nimport json,os,sys\nfrom pathlib import Path\n'
                    'name=Path(sys.argv[0]).name\n'
                    'record=Path(os.environ["TEST_TMPDIR"],name+".args")\n'
                    'if not record.exists(): record.write_text(json.dumps(sys.argv[1:]))\n'
                    'count = 0 if "missing" in sys.argv or "--ignored" in sys.argv else 1\n'
                    'case=next((a for a in sys.argv[1:] if not a.startswith("-")), name+"::case")\n'
                    'if "--list" in sys.argv:\n'
                    '    if count: print(case+": test")\n'
                    '    print(str(count)+" tests, 0 benchmarks")\n'
                    'else:\n'
                    '    print("member-output "+name)\n'
                    '    print("test result: ok. "+str(count)+" passed; 0 failed; 0 ignored; 0 measured; 1 filtered out;")\n'
                    + ('sys.exit(7 if name == "second" else 0)\n' if failing else '')
                )
                member.chmod(0o755)
            manifest = root / "manifest"
            manifest.write_text("_main/first\n_main/second\n")
            xml = root / "test.xml"
            result = subprocess.run(
                ["bash", str(RUNNER), "2", "0", "_main/first", "0", "_main/second", *args],
                cwd=root, text=True, capture_output=True, timeout=10,
                env=dict(os.environ, TEST_SRCDIR=tmp, TEST_WORKSPACE="_main",
                         TEST_TMPDIR=str(logs), LASH_BATCH_MANIFEST=str(manifest),
                         XML_OUTPUT_FILE=str(xml),
                         **({"LASH_BATCH_JOBS": jobs} if jobs is not None else {})),
            )
            self.report = ET.parse(xml).getroot() if xml.exists() else None
            if result.returncode != 0 and not (logs / "first.args").exists():
                return result, None
            observed = [json.loads((logs / (name + ".args")).read_text()) for name in ("first", "second")]
            return result, observed

    def test_filter_and_output_arguments_reach_each_member_unchanged(self):
        args = ["a filter with spaces", "--exact", "--nocapture"]
        result, observed = self.invoke(*args)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(observed, [args, args])
        self.assertIn("member-output first", result.stdout)
        self.assertIn("member-output second", result.stdout)

    def test_list_prints_member_names(self):
        result, observed = self.invoke("--list")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(observed, [["--list"], ["--list"]])
        self.assertIn("first::case: test", result.stdout)
        self.assertIn("second::case: test", result.stdout)

    def test_no_matching_tests_is_an_explicit_failure(self):
        for args in (("missing", "--exact"), ("missing", "--list")):
            with self.subTest(args=args):
                result, _ = self.invoke(*args)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("no tests matched", result.stderr)

    def test_ignored_only_selection_requires_an_execution(self):
        result, observed = self.invoke("--ignored")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(observed, [["--ignored"], ["--ignored"]])
        self.assertIn("no executable tests matched", result.stderr)

    def test_plain_success_stays_compact_and_failure_prints_its_member(self):
        success, observed = self.invoke()
        self.assertEqual(success.returncode, 0)
        self.assertEqual(observed, [[], []])
        self.assertNotIn("member-output", success.stdout)
        failed, _ = self.invoke(failing=True)
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("member-output second", failed.stderr)

    def test_report_has_one_suite_per_member_with_its_outcome(self):
        self.invoke(failing=True)
        suites = {suite.get("name"): suite for suite in self.report}
        self.assertEqual(list(suites), ["first", "second"])
        self.assertEqual(suites["first"].get("errors"), "0")
        self.assertEqual(suites["second"].get("errors"), "1")
        self.assertEqual(
            suites["second"].find("testcase/error").get("message"),
            "exited with error code 7",
        )
        self.assertIn("member-output second", suites["second"].find("system-out").text)

    def test_concurrency_comes_from_the_rule_not_the_host(self):
        for jobs in (None, "0", "two"):
            with self.subTest(jobs=jobs):
                result, observed = self.invoke(jobs=jobs)
                self.assertNotEqual(result.returncode, 0)
                self.assertIsNone(observed)
                self.assertIn("LASH_BATCH_JOBS", result.stderr)

    def test_no_more_members_run_at_once_than_the_batch_reserved(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "_main").mkdir()
            logs = root / "logs"
            logs.mkdir()
            names = [f"member{i}" for i in range(4)]
            for name in names:
                member = root / "_main" / name
                member.write_text(
                    "#!/usr/bin/env bash\n"
                    'echo start >> "$TEST_TMPDIR/events"\n'
                    "sleep 0.3\n"
                    'echo end >> "$TEST_TMPDIR/events"\n'
                )
                member.chmod(0o755)
            manifest = root / "manifest"
            manifest.write_text("".join(f"_main/{name}\n" for name in names))
            result = subprocess.run(
                ["bash", str(RUNNER), str(len(names)),
                 *(part for name in names for part in ("0", f"_main/{name}"))],
                cwd=root, text=True, capture_output=True, timeout=20,
                env=dict(os.environ, TEST_SRCDIR=tmp, TEST_WORKSPACE="_main",
                         TEST_TMPDIR=str(logs), LASH_BATCH_MANIFEST=str(manifest),
                         LASH_BATCH_JOBS="2", XML_OUTPUT_FILE=str(root / "test.xml")),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            running = peak = 0
            for event in (logs / "events").read_text().split():
                running += 1 if event == "start" else -1
                peak = max(peak, running)
            self.assertEqual(peak, 2)


class BatchMemberFormatTests(unittest.TestCase):
    """A batch member is `<n> <n NAME=value> <binary>`, as `test_batch.bzl` emits it."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def binary(self, name, body):
        path = self.root / name
        path.write_text('#!/usr/bin/env bash\n' + body)
        path.chmod(0o755)
        return str(path)

    def run_batch(self, *argv):
        env = {
            'PATH': '/usr/bin:/bin',
            'LASH_BATCH_JOBS': '2',
            'TEST_TMPDIR': str(self.root),
            'XML_OUTPUT_FILE': str(self.root / 'test.xml'),
        }
        return subprocess.run(
            ['/usr/bin/bash', str(RUNNER), *argv],
            cwd=self.root, env=env, capture_output=True, text=True, check=False,
        )

    def test_each_member_runs_with_its_own_environment(self):
        check = 'test "$CARGO_MANIFEST_DIR" = "$1" && test -z "${OTHER:-}"\n'
        first = self.binary('first', check.replace('$1', 'crates/first'))
        second = self.binary('second', check.replace('$1', 'crates/second') + 'test "$OUT_DIR" = "buck-out/out"\n')
        bare = self.binary('bare', 'test -z "${CARGO_MANIFEST_DIR:-}"\n')
        result = self.run_batch(
            '3',
            '1', 'CARGO_MANIFEST_DIR=crates/first', first,
            '2', 'CARGO_MANIFEST_DIR=crates/second', 'OUT_DIR=buck-out/out', second,
            '0', bare,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('PASS: 3 test binaries in batch', result.stdout)
        report = (self.root / 'test.xml').read_text()
        for name in (first, second, bare):
            self.assertIn(os.path.basename(name), report)

    def test_a_failing_member_fails_the_batch(self):
        passing = self.binary('passing', 'exit 0\n')
        failing = self.binary('failing', 'echo broken; exit 1\n')
        result = self.run_batch('2', '0', passing, '1', 'CARGO_PKG_NAME=x', failing)
        self.assertEqual(result.returncode, 1)
        self.assertIn('FAIL: 1 of 2 test binaries failed', result.stderr)
        self.assertIn('broken', result.stderr)

    def test_member_values_and_paths_are_never_word_split_or_expanded(self):
        directory = self.root / 'with space; $(touch pwned) *'
        directory.mkdir()
        value = 'two words; $(touch pwned) * `id` "quoted"'
        member = directory / 'member $x'
        member.write_text(
            '#!/usr/bin/env bash\n'
            'test "$VALUE" = \'' + value + '\' && test "$#" = 0\n'
        )
        member.chmod(0o755)
        result = self.run_batch('1', '1', 'VALUE=' + value, str(member))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('PASS: 1 test binaries in batch', result.stdout)
        self.assertFalse((self.root / 'pwned').exists())

    def test_a_multiline_value_stays_one_assignment(self):
        value = 'first line\nSECOND=leaked\n'
        member = self.binary(
            'member',
            'test "$VALUE" = $\'first line\\nSECOND=leaked\\n\' && test -z "${SECOND:-}" && test "$LAST" = kept\n',
        )
        result = self.run_batch('1', '2', 'VALUE=' + value, 'LAST=kept', member)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_binary_path_holding_an_equals_sign_still_runs(self):
        directory = self.root / 'mode=opt'
        directory.mkdir()
        member = directory / 'member'
        member.write_text('#!/usr/bin/env bash\ntest "$1" = "a=b" && echo ran-with-argument\nexit 3\n')
        member.chmod(0o755)
        result = self.run_batch('1', '1', 'NAME=value', str(member), 'a=b')
        self.assertEqual(result.returncode, 1)
        self.assertIn('ran-with-argument', result.stderr)

    def test_a_filter_matching_no_member_fails_the_batch(self):
        empty = (
            'echo; echo "running 0 tests"; echo\n'
            'echo "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s"\n'
        )
        first = self.binary('first', empty)
        second = self.binary('second', empty)
        result = self.run_batch('2', '0', first, '1', 'CARGO_PKG_NAME=x', second, 'no_such_case')
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn('no tests matched the batch arguments', result.stderr)

    def test_a_malformed_member_is_refused(self):
        lone = self.binary('lone', 'exit 0\n')
        result = self.run_batch('1', '2', 'A=1', lone)
        self.assertEqual(result.returncode, 1)
        self.assertIn('invalid environment count', result.stderr)



if __name__ == "__main__":
    unittest.main()
