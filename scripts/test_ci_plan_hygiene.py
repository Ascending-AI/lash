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

    def test_shallow_checkout_preserves_gate_ranges(self):
        self.git("branch", "-M", "main")
        self.git("checkout", "-qb", "pr")
        commits = []
        for name in ("first.txt", "second.txt"):
            (self.repo / name).write_text("PR addition\n")
            self.commit()
            commits.append(self.git("rev-parse", "HEAD").stdout.strip())
        pr_head = commits[-1]
        self.git("checkout", "-q", "main")
        (self.repo / "main.txt").write_text("base-only addition\n")
        self.commit()
        main_head = self.git("rev-parse", "HEAD").stdout.strip()
        self.git("checkout", "-qb", "merge-group")
        self.git("merge", "--no-ff", "-qm", "Merge PR", "pr")
        merge_head = self.git("rev-parse", "HEAD").stdout.strip()
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        for job in ("diff-hygiene", "secret-scan"):
            section = workflow.split(f"  {job}:\n", 1)[1]
            checkout = section.split("        run: |\n", 1)[1].split("\n      - name:", 1)[0]
            # Execute the workflow's actual fetch/checkout code against a local remote.
            checkout = "\n".join(line[10:] for line in checkout.splitlines())
            checkout = checkout[checkout.index("git fetch"):]
            for event, branch, base, expected in (
                ("pull_request", "pr", self.base, commits),
                ("merge_group", "merge-group", main_head, [*commits, merge_head]),
                ("push", "pr", "", commits),
            ):
                with self.subTest(job=job, event=event), tempfile.TemporaryDirectory() as temp:
                    clone = Path(temp) / "checkout"
                    subprocess.run(
                        ["git", "clone", "-q", "--depth=1", "--branch", branch,
                         self.repo.as_uri(), str(clone)], check=True, capture_output=True,
                    )
                    def git(*args):
                        return subprocess.run(["git", *args], cwd=clone, text=True,
                                              capture_output=True, check=True).stdout.strip()
                    head = merge_head if event == "merge_group" else pr_head
                    self.assertEqual("true", git("rev-parse", "--is-shallow-repository"))
                    self.assertEqual(head, git("rev-list", "HEAD"))
                    result = subprocess.run(
                        ["bash", "-euo", "pipefail", "-c", checkout], cwd=clone,
                        env={**os.environ, "BASE_SHA": base, "GITHUB_SHA": head},
                        text=True, capture_output=True,
                    )
                    self.assertEqual(0, result.returncode, result.stdout + result.stderr)
                    resolved_base = base or git("merge-base", "origin/main", "HEAD")
                    self.assertEqual(head, git("rev-parse", "HEAD"))
                    self.assertEqual(
                        ["first.txt", "second.txt"],
                        git("diff", "--diff-filter=A", "--name-only",
                            f"{resolved_base}...HEAD").splitlines(),
                    )
                    self.assertCountEqual(expected, git("rev-list", f"{resolved_base}..HEAD").splitlines())

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
