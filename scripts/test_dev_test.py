#!/usr/bin/env python3
"""Exercise selection, concurrent joins and stale-input refusal with a fake executor."""

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
        self.assertEqual(json.loads(result.stdout)["command"], ["kiln", "test", "//crates/example:all"])
        (self.source.parents[1] / "Cargo.toml").write_text("[package]\n")
        self.assertEqual(json.loads(self.invoke("--dry-run").stdout)["command"], ["kiln", "test", "//:dev_tests"])

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
        self.assertEqual(json.loads(result.stdout)["command"], ["kiln", "test", "//:dev_tests"])

    def test_concurrent_callers_share_failure_but_later_call_retries(self):
        self.env["TEST_EXIT"] = "7"
        hold = self.root / ".git/hold"
        hold.touch()
        first = self.start()
        self.wait_started()
        second = self.start()
        self.assertIn("joining", second.stdout.readline())
        hold.unlink()
        first.communicate(timeout=10)
        second.communicate(timeout=10)
        self.assertEqual((first.returncode, second.returncode), (7, 7))
        self.assertEqual((self.root / ".git/calls").read_text().splitlines(), ["run"])
        self.assertEqual(self.invoke().returncode, 7)
        self.assertEqual(len((self.root / ".git/calls").read_text().splitlines()), 2)

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

    def test_live_store_environment_is_refused(self):
        self.env["LASH_POSTGRES_DATABASE_URL"] = "postgres://fixture"
        self.assertEqual(self.invoke().returncode, 2)
        self.assertFalse((self.root / ".git/calls").exists())


if __name__ == "__main__":
    unittest.main()
