#!/usr/bin/env python3
"""The baseline scripts/ci/version-bump-gate.sh hands the strict gate."""

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
GATE = ROOT / "scripts/ci/version-bump-gate.sh"
# Stand-ins that record how the gate script called them.
STUB = 'import sys\nprint("{name}", *sys.argv[1:])\nsys.exit({code})\n'


class VersionBumpGateTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.repo = Path(directory.name)
        (self.repo / "scripts/ci").mkdir(parents=True)
        shutil.copy2(GATE, self.repo / "scripts/ci/version-bump-gate.sh")
        self.stub("check_version_bumps", 0)
        self.stub("release_baseline", 0)
        self.git("init", "--quiet", "--initial-branch=main")
        self.commits = [self.commit(f"change {index}") for index in range(3)]

    def stub(self, name, code):
        (self.repo / f"scripts/{name}.py").write_text(STUB.format(name=name, code=code))

    def git(self, *args):
        return subprocess.run(
            ["git", "-c", "user.name=gate", "-c", "user.email=gate@example.invalid", *args],
            cwd=self.repo, check=True, capture_output=True, text=True,
        ).stdout.strip()

    def commit(self, message):
        (self.repo / "tree").write_text(message)
        self.git("add", "--all")
        self.git("commit", "--quiet", "--message", message)
        return self.git("rev-parse", "HEAD")

    def gate(self, *args):
        return subprocess.run(
            ["bash", str(self.repo / "scripts/ci/version-bump-gate.sh"), *args],
            cwd=self.repo, capture_output=True, text=True,
        )

    def compared(self, result):
        lines = [line.split() for line in result.stdout.splitlines() if line.startswith("check_version_bumps ")]
        self.assertEqual(len(lines), 1, result.stdout + result.stderr)
        arguments = lines[0]
        return arguments[arguments.index("--base") + 1], arguments[arguments.index("--head") + 1]

    def test_a_named_base_is_the_baseline(self):
        base, _, head = self.commits
        result = self.gate(head, base)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.compared(result), (base, head))
        self.assertNotIn("release_baseline", result.stdout)

    def test_the_first_release_is_checked_against_the_declared_baseline(self):
        head = self.commits[-1]
        # Pre-1.0 tags are no release baseline.
        self.git("tag", "v0.1.0-alpha.9", self.commits[0])
        result = self.gate(head)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("release_baseline --repo", result.stdout)
        self.assertIn(" check", result.stdout)
        self.assertEqual(self.compared(result), (head, head))

    def test_the_last_release_is_the_baseline_and_the_candidates_own_tag_is_not(self):
        first, second, head = self.commits
        self.git("tag", "v1.0.0", first)
        self.git("tag", "v1.1.0", second)
        self.git("tag", "v1.2.0", head)
        result = self.gate(head)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.compared(result), (second, head))
        self.assertNotIn("release_baseline", result.stdout)

    def test_a_failed_gate_and_a_missed_baseline_fail_the_script(self):
        base, _, head = self.commits
        self.stub("check_version_bumps", 1)
        self.assertEqual(self.gate(head, base).returncode, 1)
        self.stub("check_version_bumps", 2)
        self.assertEqual(self.gate(head, base).returncode, 2)
        self.stub("check_version_bumps", 0)
        self.stub("release_baseline", 1)
        result = self.gate(head)
        self.assertEqual(result.returncode, 1)
        self.assertNotIn("check_version_bumps", result.stdout)

    def test_an_unresolvable_commit_fails(self):
        self.assertNotEqual(self.gate("0" * 40).returncode, 0)
        self.assertNotEqual(self.gate(self.commits[-1], "0" * 40).returncode, 0)
        self.assertEqual(self.gate().returncode, 2)


if __name__ == "__main__":
    unittest.main()
