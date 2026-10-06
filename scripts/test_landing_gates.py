#!/usr/bin/env python3
"""Landing verdicts against reset sources.

The scratch repository owns the version comparison. No service, reset-tree
build or CI dispatch runs.
"""

from pathlib import Path
import os
import shutil
import subprocess
import tarfile
import tempfile
import unittest

import release_baseline as baseline
import release_reset as reset

ROOT = Path(__file__).resolve().parents[1]
GATE = "scripts/ci/landing-gates.sh"
VERSION_GATE = "scripts/ci/version-bump-gate.sh"
PROOF = ROOT / ".buck2/landing-proof"


class LandingGatesTests(unittest.TestCase):
    def setUp(self):
        PROOF.mkdir(parents=True, exist_ok=True)
        temporary = tempfile.TemporaryDirectory(dir=PROOF)
        self.addCleanup(temporary.cleanup)
        self.repo = Path(temporary.name)
        self.env = dict(os.environ)

    def git(self, *args):
        return subprocess.run(
            ["git", "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid",
             "-c", "commit.gpgsign=false", "-c", "gc.auto=0", *args],
            cwd=self.repo, check=True, capture_output=True, text=True,
        ).stdout.strip()

    def commit(self, message):
        self.git("add", "--all")
        self.git("commit", "--quiet", "--allow-empty", "--message", message)
        return self.git("rev-parse", "HEAD")

    def invoke(self, *args):
        return subprocess.run(
            ["bash", str(self.repo / GATE), *args], cwd=self.repo,
            env=self.env, capture_output=True, text=True,
        )

    def copy_gates(self):
        for relative in (GATE, VERSION_GATE):
            source = ROOT / relative
            if source.exists():
                destination = self.repo / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, destination)

    def post_reset_repo(self):
        archive = PROOF / "source.tar"
        with archive.open("wb") as output:
            subprocess.run(["git", "archive", "HEAD"], cwd=ROOT, stdout=output, check=True)
        with tarfile.open(archive) as source:
            source.extractall(self.repo, filter="data")
        self.copy_gates()
        _, edits = reset.plan(self.repo)
        for path, text in edits.items():
            path.write_text(text)
        self.assertEqual(baseline.mismatches(baseline.inventory(self.repo)), [])
        self.git("init", "--quiet", "--initial-branch=main")
        return self.commit("post-reset baseline")

    def test_post_reset_unbumped_shape_fails(self):
        base = self.post_reset_repo()
        shape = self.repo / "crates/lash-remote-protocol/src/turn_result.rs"
        original = shape.read_text()
        self.assertIn("pub struct RemoteTurnReport {", original)
        shape.write_text(original.replace("pub struct RemoteTurnReport {",
                                          "pub struct RemoteTurnReport {\n    pub planted: String,", 1))
        head = self.commit("unbumped guarded shape")
        unbumped = self.invoke(base, head)
        (PROOF / "unbumped.log").write_text(unbumped.stdout + unbumped.stderr)
        self.assertEqual(unbumped.returncode, 1, unbumped.stdout + unbumped.stderr)
        self.assertIn("version-bump check failed", unbumped.stderr)
        self.assertIn("REMOTE_PROTOCOL_VERSION is 1 on both sides", unbumped.stderr)
        print("post-reset unbumped guarded edit: exit 1")

    def minimal_repo(self):
        self.copy_gates()
        self.git("init", "--quiet", "--initial-branch=main")
        (self.repo / "tracked").write_text("baseline\n")
        return self.commit("baseline")

    def test_an_unresolvable_baseline_fails(self):
        head = self.minimal_repo()
        result = self.invoke("0" * 40, head)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fatal", result.stderr)

    def test_a_different_candidate_checkout_fails(self):
        base = self.minimal_repo()
        head = self.commit("candidate")
        result = self.invoke(base, base)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("candidate must be checked out", result.stderr)
        self.assertNotEqual(base, head)

    def test_a_dirty_candidate_checkout_fails(self):
        head = self.minimal_repo()
        (self.repo / "tracked").write_text("dirty\n")
        result = self.invoke(head, head)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tracked edits", result.stderr)


if __name__ == "__main__":
    unittest.main()
