#!/usr/bin/env python3
"""Exercise selection, concurrent serialization and stale-snapshot refusal with a fake executor."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest


SOURCE = Path(__file__).with_name("dev-test.py")


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
            '#!/usr/bin/env python3\nimport os,time\nfrom pathlib import Path\n'
            'root=Path.cwd()/".git"\n'
            'with (root/"calls").open("a") as f: f.write("run\\n")\n'
            '(root/"pid").write_text(str(os.getpid()))\n'
            '(root/"started").touch()\n'
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

    def test_query_cannot_select_deferred_manual_or_duplicate_batch_members(self):
        query = self.bin / "bazel"
        query.write_text("#!/bin/sh\nprintf '%s\\n' //crates/example:first //crates/example:second "
                         "//crates/example:test_batch //crates/example:deferred //crates/example:manual\n")
        query.chmod(0o755)
        commands = json.loads(self.invoke("--dependents", "--dry-run").stdout)["commands"]
        self.assertEqual(commands, [["kiln", "test", "//crates/example:test_batch", "//:schema_checks"]])
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


if __name__ == "__main__":
    unittest.main()
