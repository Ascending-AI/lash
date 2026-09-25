#!/usr/bin/env python3
"""Exercise selection, concurrent serialization and stale-snapshot refusal with a fake executor."""

import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest


SOURCE = Path(__file__).with_name("dev-test.py")


def dev_test_module():
    """`dev-test.py` loaded as a module, for its pure summary helpers."""
    spec = importlib.util.spec_from_file_location("dev_test_script", SOURCE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class DevTestTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("LASH_", "KILN_", "BAZEL_", "CARGO_", "GIT_"))
        }
        self.env.update({
            "GIT_AUTHOR_NAME": "Test", "GIT_AUTHOR_EMAIL": "test@example.test",
            "GIT_COMMITTER_NAME": "Test", "GIT_COMMITTER_EMAIL": "test@example.test",
        })
        (self.root / "scripts").mkdir()
        shutil.copyfile(SOURCE, self.root / "scripts/dev-test.py")
        shutil.copyfile(SOURCE.with_name("ci_plan.py"), self.root / "scripts/ci_plan.py")
        self.source = self.root / "crates/example/src/lib.rs"
        self.source.parent.mkdir(parents=True)
        self.source.write_text("pub fn example() {}\n")
        (self.source.parents[1] / "BUILD.bazel").write_text("# fixture\n")
        inventory = self.root / "tools/bazel/workspace_targets.bzl"
        inventory.parent.mkdir(parents=True)
        inventory.write_text(
            'WORKSPACE_DEV_TEST_TARGETS = ["//crates/example:first", "//crates/example:second"]\n'
            'WORKSPACE_TEST_BATCHES = {"//crates/example:test_batch": ["//crates/example:first", "//crates/example:second"]}\n'
        )
        # The checked-in label inventory `ci_plan.pr_tail_labels` reads.
        (self.root / "tools/bazel/target-inventory.json").write_text(json.dumps({"packages": [
            {"manifest": "crates/example/Cargo.toml", "targets": [
                {"kind": "test", "label": "//crates/example:first", "tags": []},
                {"kind": "test", "label": "//crates/example:second", "tags": []},
            ]},
            {"manifest": "crates/slow/Cargo.toml", "targets": [
                {"kind": "test", "label": "//crates/slow:slow__test", "tags": ["dev-deferred"]},
                {"kind": "test", "label": "//crates/slow:service__test", "tags": ["manual"]},
                {"kind": "test", "label": "//crates/slow:trunk__test", "tags": ["pr-deferred"]},
            ]},
            {"manifest": "examples/sample/Cargo.toml", "targets": [
                {"kind": "test", "label": "//examples/sample:leaf__test", "tags": ["dev-deferred"]},
            ]},
        ]}))
        # A package whose only Bazel test label is dev-deferred.
        slow = self.root / "crates/slow/src"
        slow.mkdir(parents=True)
        (slow / "lib.rs").write_text("pub fn slow() {}\n")
        (self.root / "crates/slow/BUILD.bazel").write_text("# fixture\n")
        workflow = self.root / ".github/workflows/ci.yml"
        workflow.parent.mkdir(parents=True)
        workflow.write_text("bash scripts/ci/run-gate-commands.sh --jobs 4 <<'GATES'\n"
                            "python3 scripts/test_dev_test.py\nGATES\n")
        (self.root / "scripts/test_dev_test.py").write_text("raise SystemExit(0)\n")
        (self.root / ".gitignore").write_text(".kiln.bazelrc\n")
        (self.root / ".kiln.bazelrc").touch()
        self.git("init", "-q")
        self.git("add", ".")
        self.git("commit", "-qm", "fixture")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        self.source.write_text("pub fn example() { let _ = 1; }\n")
        self.bin = self.root / ".git/bin"
        self.bin.mkdir()
        executor = self.bin / "kiln"
        executor.write_text(
            '#!/usr/bin/env python3\nimport os,time,sys\nfrom pathlib import Path\n'
            'root=Path.cwd()/".git"\n'
            'with (root/"calls").open("a") as f: f.write("run\\n")\n'
            '(root/"pid").write_text(str(os.getpid()))\n'
            '(root/"started").touch()\n'
            'canned=os.environ.get("TEST_STDOUT_FILE")\n'
            'if canned: sys.stdout.write(Path(canned).read_text())\n'
            'while (root/"hold").exists(): time.sleep(0.01)\n'
            'raise SystemExit(int(os.environ.get("TEST_EXIT", "0")))\n'
        )
        executor.chmod(0o755)
        self.env["PATH"] = str(self.bin) + os.pathsep + self.env["PATH"]

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, env=self.env)

    def command(self, *args):
        return ["python3", str(self.root / "scripts/dev-test.py"), *args]

    def invoke(self, *args):
        return subprocess.run(self.command(*args), cwd=self.root, env=self.env,
                              capture_output=True, text=True, timeout=20)

    def start(self):
        process = subprocess.Popen(self.command(), cwd=self.root, env=self.env,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(lambda: process.kill() if process.poll() is None else None)
        return process

    def wait_started(self):
        deadline = time.monotonic() + 10
        while not (self.root / ".git/started").exists():
            if time.monotonic() > deadline:
                self.fail("executor never started")
            time.sleep(0.01)

    def test_worktree_edits_and_shared_manifest_selection(self):
        result = self.invoke("--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["commands"], [["kiln", "test", "//crates/example:test_batch", "//:schema_checks"]])
        (self.source.parents[1] / "Cargo.toml").write_text("[package]\n")
        self.assertEqual(json.loads(self.invoke("--dry-run").stdout)["commands"], [["kiln", "test", "//:dev_tests", "//:schema_checks"]])

    def test_a_package_diff_also_selects_its_dev_deferred_labels(self):
        (self.root / "crates/slow/src/lib.rs").write_text("pub fn slow() { let _ = 1; }\n")
        commands = json.loads(self.invoke("--dry-run").stdout)["commands"]
        self.assertEqual(commands, [[
            "kiln", "test",
            "//crates/example:test_batch",
            "//crates/slow:slow__test",
            "//:schema_checks",
        ]])
        # The manual and pr-deferred labels of the same package stay out; so
        # does the dev-deferred examples leaf. A package manifest widens to
        # the suite but is still a deferred test's input.
        (self.root / "crates/slow/Cargo.toml").write_text("[package]\n")
        commands = json.loads(self.invoke("--dry-run").stdout)["commands"]
        self.assertEqual(commands, [[
            "kiln", "test",
            "//:dev_tests",
            "//crates/slow:slow__test",
            "//:schema_checks",
        ]])

    def test_query_cannot_select_deferred_manual_or_duplicate_batch_members(self):
        query = self.bin / "bazel"
        query.write_text("#!/bin/sh\nprintf '%s\\n' \"$*\" > .git/query-args\nprintf '%s\\n' //crates/example:first //crates/example:second "
                         "//crates/example:test_batch //crates/example:deferred //crates/example:manual\n")
        query.chmod(0o755)
        commands = json.loads(self.invoke("--dependents", "--dry-run").stdout)["commands"]
        self.assertEqual(commands, [["kiln", "test", "//crates/example:test_batch", "//:schema_checks"]])
        self.assertIn(
            "rdeps(set(//crates/... //examples/... //runbooks/...),",
            (self.root / ".git/query-args").read_text(),
        )
        query.write_text("#!/bin/sh\necho //crates/example:first\n")
        self.assertEqual(json.loads(self.invoke("--dependents", "--dry-run").stdout)["commands"],
                         [["kiln", "test", "//crates/example:first", "//:schema_checks"]])

    def test_known_script_edit_runs_its_ci_proof_and_propagates_failure(self):
        self.git("checkout", "--", "crates/example/src/lib.rs")
        (self.root / "scripts/test_dev_test.py").write_text("raise SystemExit(7)\n")
        planned = json.loads(self.invoke("--dry-run").stdout)
        self.assertEqual(planned["commands"], [["python3", "scripts/test_dev_test.py"]])
        self.assertEqual(self.invoke().returncode, 7)
        self.assertFalse((self.root / ".git/calls").exists())
        receipt = json.loads((self.root / ".git/lash-validation/latest.json").read_text())
        self.assertEqual(receipt["exit_code"], 7)

    def test_shared_tooling_runs_repository_proof_and_rust_suite(self):
        (self.root / "scripts/unknown.py").write_text("# new tooling\n")
        self.assertEqual(json.loads(self.invoke("--dry-run").stdout)["commands"], [
            ["bash", "scripts/ci/repository-gates.sh"],
            ["kiln", "test", "//:dev_tests", "//:schema_checks"],
        ])

    def test_service_only_package_builds_without_running_manual_tests(self):
        self.git("checkout", "--", "crates/example/src/lib.rs")
        package = self.root / "crates/service"
        package.mkdir()
        (package / "BUILD.bazel").write_text("# fixture\n")
        self.git("add", "crates/service/BUILD.bazel")
        self.git("commit", "-qm", "service fixture")
        self.git("update-ref", "refs/remotes/origin/main", "HEAD")
        (package / "service.rs").write_text("// service change\n")
        self.assertEqual(json.loads(self.invoke("--dry-run").stdout)["commands"], [
            ["kiln", "build", "//crates/service:all", "//:schema_checks"],
        ])
        self.source.write_text("pub fn example() { let _ = 2; }\n")
        self.assertEqual(json.loads(self.invoke("--dry-run").stdout)["commands"], [
            ["kiln", "build", "//crates/service:all", "//:schema_checks"],
            ["kiln", "test", "//crates/example:test_batch", "//:schema_checks"],
        ])

    def test_facade_keeps_explicit_manual_seal(self):
        (self.root / "Cargo.toml").write_text("[workspace]\n")
        self.assertEqual(json.loads(self.invoke("--dry-run").stdout)["commands"], [
            ["kiln", "test", "//:dev_tests", "//crates/lash:ui_fixtures", "//:schema_checks"],
        ])

    def test_untracked_content_changes_identity(self):
        untracked = self.root / "crates/example/src/new.rs"
        untracked.write_text("first")
        before = json.loads(self.invoke("--dry-run").stdout)["inputs"]
        untracked.write_text("second")
        self.assertNotEqual(before, json.loads(self.invoke("--dry-run").stdout)["inputs"])

    def test_failed_query_widens_selection(self):
        query = self.bin / "bazel"
        query.write_text("#!/bin/sh\nexit 7\n")
        query.chmod(0o755)
        result = self.invoke("--dependents", "--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["commands"], [["kiln", "test", "//:dev_tests", "//:schema_checks"]])

    def test_waiting_callers_recheck_bazel_inputs_instead_of_reusing_receipts(self):
        self.env["TEST_EXIT"] = "7"
        hold = self.root / ".git/hold"
        hold.touch()
        first = self.start()
        self.wait_started()
        second = self.start()
        self.assertIn("waiting", second.stdout.readline())
        hold.unlink()
        first.communicate(timeout=10)
        second.communicate(timeout=10)
        self.assertEqual((first.returncode, second.returncode), (7, 7))
        self.assertEqual((self.root / ".git/calls").read_text().splitlines(), ["run", "run"])
        self.assertEqual(self.invoke().returncode, 7)
        self.assertEqual(len((self.root / ".git/calls").read_text().splitlines()), 3)

    def test_edit_during_validation_cannot_produce_green_receipt(self):
        hold = self.root / ".git/hold"
        hold.touch()
        process = self.start()
        self.wait_started()
        self.source.write_text("changed during validation\n")
        hold.unlink()
        process.communicate(timeout=10)
        self.assertEqual(process.returncode, 2)
        receipt = json.loads((self.root / ".git/lash-validation/latest.json").read_text())
        self.assertFalse(receipt["inputs_unchanged"])


    def test_ignored_input_change_still_requires_each_waiter_to_invoke_bazel(self):
        ignore = self.root / ".gitignore"
        ignore.write_text(ignore.read_text() + "generated-input\n")
        ignored = self.root / "generated-input"
        ignored.write_text("first\n")
        hold = self.root / ".git/hold"
        hold.touch()
        first = self.start()
        self.wait_started()
        second = self.start()
        self.assertIn("waiting", second.stdout.readline())
        ignored.write_text("second\n")
        hold.unlink()
        first.communicate(timeout=10)
        second.communicate(timeout=10)
        self.assertEqual((first.returncode, second.returncode), (0, 0))
        self.assertEqual((self.root / ".git/calls").read_text().splitlines(), ["run", "run"])
        receipt = json.loads((self.root / ".git/lash-validation/latest.json").read_text())
        self.assertIn("checkout/config snapshot", receipt["plan"]["identity_scope"])


    def test_interrupt_stops_the_owned_executor_and_writes_failure(self):
        hold = self.root / ".git/hold"
        hold.touch()
        process = self.start()
        self.wait_started()
        child_pid = int((self.root / ".git/pid").read_text())
        process.terminate()
        process.communicate(timeout=15)
        self.assertEqual(process.returncode, 130)
        with self.assertRaises(ProcessLookupError):
            os.kill(child_pid, 0)
        receipt = json.loads((self.root / ".git/lash-validation/latest.json").read_text())
        self.assertEqual(receipt["exit_code"], 130)

    def test_live_store_environment_is_refused(self):
        self.env["LASH_POSTGRES_DATABASE_URL"] = "postgres://fixture"
        self.assertEqual(self.invoke().returncode, 2)
        self.assertFalse((self.root / ".git/calls").exists())

    def canned_failure(self):
        """A fake `kiln test` failure: one target fails with a Rust panic."""
        # The canned artifacts live outside the fixture root: an untracked
        # file inside it would widen the plan's selection.
        outside = tempfile.TemporaryDirectory()
        self.addCleanup(outside.cleanup)
        artifacts = Path(outside.name) / "testlogs/crates/example/first"
        artifacts.mkdir(parents=True)
        (artifacts / "test.log").write_text(
            "exec ${PAGER:-less} \"$0\" || exit 1\n"
            "Executed tests from //crates/example:first\n"
            "running 3 tests\n"
            "test alpha::passes ... ok\n"
            "test alpha::fails_hard ... FAILED\n"
            "test alpha::passes_too ... ok\n\n"
            "failures:\n\n"
            "---- alpha::fails_hard stdout ----\n"
            "thread 'alpha::fails_hard' panicked at crates/example/src/lib.rs:42:7:\n"
            "assertion `left == right` failed\n"
            "  left: 1\n"
            " right: 2\n"
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n"
            "failures:\n"
            "    alpha::fails_hard\n\n"
            "test result: FAILED. 2 passed; 1 failed; 0 ignored\n"
        )
        (artifacts / "test.xml").write_text(
            '<?xml version="1.0" encoding="UTF-8"?>\n'
            '<testsuite name="//crates/example:first" tests="3" failures="1">\n'
            '  <testcase name="alpha::passes" classname="first" time="0.01"/>\n'
            '  <testcase name="alpha::fails_hard" classname="first" time="0.02">\n'
            '    <failure message="panicked">thread \'alpha::fails_hard\' panicked at '
            'crates/example/src/lib.rs:42:7:\nassertion `left == right` failed</failure>\n'
            '  </testcase>\n'
            '  <testcase name="alpha::passes_too" classname="first" time="0.01"/>\n'
            '</testsuite>\n'
        )
        canned = Path(outside.name) / "canned.txt"
        canned.write_text(
            "INFO: Invocation ID: fixture\n"
            "BAZEL-NOISE-MARKER-LINE\n"
            "==================== Test output for //crates/example:first:\n"
            + "noise padding lines that must never reach stdout\n" * 8
            + "-----------------------------------------------------------------------------\n"
            f"//crates/example:first                     FAILED in 1.2s\n"
            f"  {artifacts}/test.log\n\n"
            f"//crates/example:second                    PASSED in 0.3s\n"
            f"  {artifacts}/second/test.log\n\n"
            "Executed 2 out of 2 tests: 1 test fails.\n"
        )
        self.env["TEST_STDOUT_FILE"] = str(canned)
        self.env["TEST_EXIT"] = "1"

    def test_failure_summary_names_the_target_test_panic_and_log(self):
        self.canned_failure()
        result = self.invoke()
        self.assertEqual(result.returncode, 1)
        out = result.stdout
        self.assertIn("//crates/example:first", out)
        self.assertIn("alpha::fails_hard", out)
        self.assertIn("crates/example/src/lib.rs:42:7", out)
        self.assertIn("test.log", out)
        self.assertIn("--verbose", out)
        # The point of the summary: the firehose stays out of the output.
        self.assertNotIn("BAZEL-NOISE-MARKER-LINE", out)
        self.assertNotIn("noise padding", out)
        self.assertNotIn("//crates/example:second", out)
        self.assertLess(len(out.splitlines()), 60)

    def test_verbose_streams_the_full_output(self):
        self.canned_failure()
        result = self.invoke("--verbose")
        self.assertEqual(result.returncode, 1)
        self.assertIn("BAZEL-NOISE-MARKER-LINE", result.stdout)
        self.assertNotIn("failing targets:", result.stdout)

    def test_quick_mode_forwards_test_env_and_shard_includes(self):
        package = self.root / "crates/lash-typescript"
        package.mkdir()
        (package / "BUILD.bazel").write_text("# fixture\n")
        outcomes = self.root / "crates/lash-typescript/tests/test262/outcomes"
        outcomes.mkdir(parents=True)
        (outcomes / "built-ins.tsv").write_text("# rows\n")
        vendored = self.root / "crates/lash-typescript/tests/test262/test/language/statements"
        vendored.mkdir(parents=True)
        (vendored / "for.js").write_text("finish(true);\n")
        self.env["LASH_QUICK"] = "1"
        commands = json.loads(self.invoke("--dry-run").stdout)["commands"]
        test_commands = [c for c in commands if c[:2] == ["kiln", "test"]]
        self.assertTrue(test_commands)
        flags = [arg for c in test_commands for arg in c if arg.startswith("--test_env=")]
        self.assertIn("--test_env=LASH_QUICK=1", flags)
        self.assertIn(
            "--test_env=LASH_TEST262_QUICK_INCLUDE=built-ins,language", flags)
        # A selection-wide input keeps every shard whole.
        (self.root / "crates/lash-typescript/tests/test262/census.tsv").write_text("# census\n")
        flags = [
            arg
            for c in json.loads(self.invoke("--dry-run").stdout)["commands"]
            for arg in c if arg.startswith("--test_env=")
        ]
        self.assertIn(
            "--test_env=LASH_TEST262_QUICK_INCLUDE=*", flags)
        # Without the knob the commands carry no quick flags at all.
        del self.env["LASH_QUICK"]
        commands = json.loads(self.invoke("--dry-run").stdout)["commands"]
        self.assertFalse(
            any(arg.startswith("--test_env=") for c in commands for arg in c))


class FormatterTests(unittest.TestCase):
    """The pure summary helpers of `dev-test.py`, exercised without a run."""

    module = None

    @classmethod
    def setUpClass(cls):
        cls.module = dev_test_module()

    def test_quick_includes_map_changed_paths_to_shards(self):
        includes = self.module.quick_test262_includes
        self.assertEqual(
            includes(["crates/lash-typescript/tests/test262/test/language/foo/a.js"]),
            {"language"})
        self.assertEqual(
            includes(["crates/lash-typescript/tests/test262/outcomes/built-ins.tsv"]),
            {"built-ins"})
        self.assertEqual(
            includes(["crates/lash-typescript/tests/test262/census.tsv",
                      "crates/lash-typescript/tests/test262/test/language/a.js",
                      "crates/other/src/lib.rs"]),
            {"*"})

    def test_failed_targets_reads_the_summary_block(self):
        text = (
            "INFO: found 2 targets\n"
            "//pkg:a                                         FAILED in 1.2s\n"
            "  /execroot/out/testlogs/pkg/a/test.log\n"
            "  /execroot/out/testlogs/pkg/a/test.xml\n"
            "//pkg:b                                         PASSED in 0.3s\n"
            "FAIL: //pkg:c (see /execroot/out/testlogs/pkg/c/test.log)\n"
            "Executed 3 out of 3 tests: 2 fail.\n"
        )
        self.assertEqual(self.module.failed_targets(text), [
            ("//pkg:a", ["/execroot/out/testlogs/pkg/a/test.log",
                        "/execroot/out/testlogs/pkg/a/test.xml"]),
            ("//pkg:c", ["/execroot/out/testlogs/pkg/c/test.log"]),
        ])

    def test_panic_line_prefers_the_first_location(self):
        text = (
            "thread 'x' panicked at src/a.rs:7:9:\n"
            "assertion failed: left == right\n"
            "thread 'y' panicked at src/b.rs:9:1:\n"
            "other\n"
        )
        self.assertEqual(self.module.panic_line(text),
                         "src/a.rs:7:9: assertion failed: left == right")

    def test_failure_summary_reads_xml_and_caps_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            artifacts = Path(tmp) / "testlogs/pkg/a"
            artifacts.mkdir(parents=True)
            (artifacts / "test.log").write_text("running 1 test\n")
            (artifacts / "test.xml").write_text(
                '<testsuite><testcase name="a::boom">'
                '<failure>thread \'a::boom\' panicked at src/x.rs:3:1:\nboom</failure>'
                "</testcase></testsuite>")
            log = Path(tmp) / "command-0.log"
            log.write_text(
                "//pkg:a                       FAILED in 0.1s\n"
                f"  {artifacts}/test.log\n"
            )
            summary = self.module.failure_summary(
                ["kiln", "test", "//pkg:a"], 1, log)
            self.assertIn("//pkg:a", summary)
            self.assertIn("a::boom", summary)
            self.assertIn("src/x.rs:3:1: boom", summary)
            self.assertIn(str(artifacts / "test.log"), summary)
            self.assertLessEqual(len(summary.splitlines()), self.module.SUMMARY_LINES)

    def test_failure_summary_falls_back_to_output_tail(self):
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "command-0.log"
            log.write_text("line of noise\n" * 100 + "real error at the end\n")
            summary = self.module.failure_summary(["tool"], 2, log)
            self.assertIn("real error at the end", summary)
            self.assertNotIn("line of noise\n" * 10, summary)
            self.assertLessEqual(len(summary.splitlines()), self.module.SUMMARY_LINES)


if __name__ == "__main__":
    unittest.main()
