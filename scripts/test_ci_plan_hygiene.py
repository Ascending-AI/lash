#!/usr/bin/env python3
"""Exercise merge hygiene against synthetic commits, without touching the checkout."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
GITLEAKS = os.environ.get("GITLEAKS_BINARY") or shutil.which("gitleaks")


class HygieneTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=ROOT)
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Samuel Galanakis")
        self.git("config", "user.email", "47306720+SamGalanakis@users.noreply.github.com")
        scripts = self.repo / "scripts/ci"
        scripts.mkdir(parents=True)
        shutil.copy(ROOT / "scripts/ci/check-diff-hygiene.sh", scripts)
        (scripts / "diff-hygiene-allowlist.txt").write_text("")
        self.commit()
        self.base = self.git("rev-parse", "HEAD").stdout.strip()

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.repo, text=True, capture_output=True, check=True)

    def commit(self):
        self.git("add", ".")
        self.git("commit", "-qm", "Add fixture")

    def test_added_ds_store_is_refused(self):
        (self.repo / ".DS_Store").write_bytes(b"synthetic junk")
        self.commit()
        result = subprocess.run(
            ["bash", "scripts/ci/check-diff-hygiene.sh"], cwd=self.repo,
            env={**os.environ, "BASE_SHA": self.base, "DIFF_HYGIENE_BYPASS": "0"},
            text=True, capture_output=True,
        )
        self.assertEqual(1, result.returncode, result.stdout + result.stderr)
        self.assertIn("Check A (junk-path ancestry denylist) failed for '.DS_Store'", result.stderr)

    def test_stacked_child_scans_only_its_own_range(self):
        (self.repo / ".DS_Store").write_bytes(b"parent-only junk")
        self.commit()
        parent = self.git("rev-parse", "HEAD").stdout.strip()
        (self.repo / "child.txt").write_text("child-only change\n")
        self.commit()
        result = subprocess.run(
            ["bash", "scripts/ci/check-diff-hygiene.sh"], cwd=self.repo,
            env={**os.environ, "BASE_SHA": parent, "DIFF_HYGIENE_BYPASS": "0"},
            text=True, capture_output=True,
        )
        self.assertEqual(0, result.returncode, result.stdout + result.stderr)

    @unittest.skipUnless(GITLEAKS, "pinned Gitleaks is supplied by the secret-scan job")
    def test_added_secret_is_refused(self):
        # Construct a fake token at runtime so the regression fixture is not a leak.
        (self.repo / "config.txt").write_text('github_token = "' + "ghp_" + "Ab3dEf6hIj9kLm2nOp5qRs8tUv1wXy4zAb7C" + '"\n')
        self.commit()
        result = subprocess.run(
            [GITLEAKS, "git", f"--log-opts={self.base}..HEAD", "--redact", "--verbose"],
            cwd=self.repo, text=True, capture_output=True,
        )
        self.assertEqual(1, result.returncode, result.stdout + result.stderr)
        self.assertIn("github-pat", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
