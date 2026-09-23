#!/usr/bin/env python3
"""Exercise batch argument and result reporting with cheap libtest-shaped members."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


RUNNER = Path(__file__).resolve().parents[1] / "tools/bazel/test_batch_runner.sh"


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
                    'Path(os.environ["TEST_TMPDIR"],name+".args").write_text(json.dumps(sys.argv[1:]))\n'
                    'count = 0 if "missing" in sys.argv else 1\n'
                    'if "--list" in sys.argv:\n'
                    '    if count: print(name+"::case: test")\n'
                    '    print(str(count)+" tests, 0 benchmarks")\n'
                    'else:\n'
                    '    print("member-output "+name)\n'
                    '    print("test result: ok. "+str(count)+" passed; 0 failed; 0 ignored; 0 measured; 1 filtered out;")\n'
                    + ('sys.exit(7 if name == "second" else 0)\n' if failing else '')
                )
                member.chmod(0o755)
            manifest = root / "manifest"
            manifest.write_text("_main/first\n_main/second\n")
            result = subprocess.run(
                ["bash", str(RUNNER), *args], text=True, capture_output=True, timeout=10,
                env=dict(os.environ, TEST_SRCDIR=tmp, TEST_WORKSPACE="_main",
                         TEST_TMPDIR=str(logs), LASH_BATCH_MANIFEST=str(manifest),
                         **({"LASH_BATCH_JOBS": jobs} if jobs is not None else {})),
            )
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

    def test_plain_success_stays_compact_and_failure_prints_its_member(self):
        success, observed = self.invoke()
        self.assertEqual(success.returncode, 0)
        self.assertEqual(observed, [[], []])
        self.assertNotIn("member-output", success.stdout)
        failed, _ = self.invoke(failing=True)
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("member-output second", failed.stderr)

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
                ["bash", str(RUNNER)], text=True, capture_output=True, timeout=20,
                env=dict(os.environ, TEST_SRCDIR=tmp, TEST_WORKSPACE="_main",
                         TEST_TMPDIR=str(logs), LASH_BATCH_MANIFEST=str(manifest),
                         LASH_BATCH_JOBS="2"),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            running = peak = 0
            for event in (logs / "events").read_text().split():
                running += 1 if event == "start" else -1
                peak = max(peak, running)
            self.assertEqual(peak, 2)


if __name__ == "__main__":
    unittest.main()
