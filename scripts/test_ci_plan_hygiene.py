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
            # `github_sha` is what the event puts in GITHUB_SHA; on pull_request
            # that is the ephemeral refs/pull/N/merge commit, which also carries
            # main's tip as a parent. `scan_head` is the commit the gate must
            # actually scan. `base` is `pull_request.base.sha`, i.e. main's tip
            # now, which is deliberately NOT an ancestor of the head here: the
            # gate has to fall back on the merge base or it drags main's own
            # commits into the range.
            for event, branch, base, github_sha, scan_head, expected in (
                ("pull_request", "merge-group", main_head, merge_head, pr_head, commits),
                ("merge_group", "merge-group", main_head, merge_head, merge_head,
                 [*commits, merge_head]),
                ("push", "pr", "", pr_head, pr_head, commits),
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
                    self.assertEqual("true", git("rev-parse", "--is-shallow-repository"))
                    self.assertEqual(github_sha, git("rev-list", "HEAD"))
                    result = subprocess.run(
                        ["bash", "-euo", "pipefail", "-c", checkout], cwd=clone,
                        env={**os.environ, "BASE_SHA": base, "GITHUB_SHA": github_sha,
                             "SCAN_HEAD_SHA": scan_head},
                        text=True, capture_output=True,
                    )
                    self.assertEqual(0, result.returncode, result.stdout + result.stderr)
                    resolved_base = git("merge-base", base or "origin/main", "HEAD")
                    self.assertEqual(scan_head, git("rev-parse", "HEAD"))
                    self.assertEqual(
                        ["first.txt", "second.txt"],
                        git("diff", "--diff-filter=A", "--name-only",
                            f"{resolved_base}...HEAD").splitlines(),
                    )
                    self.assertCountEqual(expected, git("rev-list", f"{resolved_base}..HEAD").splitlines())

    def test_fuzz_corpus_seeds_are_exempt_from_added_file_count_only(self):
        corpus = self.repo / "fuzz/corpus/target_a"
        corpus.mkdir(parents=True)
        for i in range(201):
            (corpus / f"seed-{i:04}").write_bytes(b"s")
        self.commit()
        result = subprocess.run(
            ["bash", "scripts/ci/check-diff-hygiene.sh"], cwd=self.repo,
            env={**os.environ, "BASE_SHA": self.base, "DIFF_HYGIENE_BYPASS": "0"},
            text=True, capture_output=True,
        )
        self.assertEqual(0, result.returncode, result.stdout + result.stderr)

    def test_bulk_added_files_outside_fuzz_corpus_still_fail_check_c(self):
        bulk = self.repo / "vendored"
        bulk.mkdir()
        for i in range(201):
            (bulk / f"file-{i:04}.txt").write_text("x")
        self.commit()
        result = subprocess.run(
            ["bash", "scripts/ci/check-diff-hygiene.sh"], cwd=self.repo,
            env={**os.environ, "BASE_SHA": self.base, "DIFF_HYGIENE_BYPASS": "0"},
            text=True, capture_output=True,
        )
        self.assertEqual(1, result.returncode, result.stdout + result.stderr)
        self.assertIn("Check C (added-file count) failed", result.stderr)

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

    def checkout_script(self, job):
        """The named job's real checkout code, from `git fetch` onward."""
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        section = workflow.split(f"  {job}:\n", 1)[1]
        script = section.split("        run: |\n", 1)[1].split("\n      - name:", 1)[0]
        script = "\n".join(line[10:] for line in script.splitlines())
        return script[script.index("git fetch"):]

    @unittest.skipUnless(GITLEAKS, "pinned Gitleaks is supplied by the secret-scan job")
    def test_merges_of_main_do_not_leak_main_secrets(self):
        # A branch that merged main in earlier reaches main through a second
        # chain rooted far down it. `--deepen` extends every boundary, so that
        # chain exposes main commits the base's own boundary never covers, and
        # a merge base existing does not mean the shared ancestry is connected.
        # The numbers matter: the deepen loop starts at 50, so the planted main
        # commit sits more than 50 below main's tip and less than 50 below the
        # commit the branch merged earlier.
        token = "ghp_" + "Cd4eFg7iJk0lMn3oPq6rSt9uVw2xYz5aBc8D"
        (self.repo / "main-secret.txt").write_text(f'github_token = "{token}"\n')
        build = """
        set -euo pipefail
        git branch -M main
        for i in $(seq 1 59); do git commit -q --allow-empty -m "main ${i}"; done
        git add main-secret.txt
        git commit -qm "main 60 (plants a credential on main)"
        for i in $(seq 61 65); do git commit -q --allow-empty -m "main ${i}"; done
        git rev-parse HEAD > .early-main
        for i in $(seq 66 120); do git commit -q --allow-empty -m "main ${i}"; done
        git checkout -q -b pr "$(cat .base)"
        echo one > pr-one.txt; git add pr-one.txt; git commit -qm "pr 1"
        git merge -q --no-ff -m "Merge main into pr (early)" "$(cat .early-main)"
        echo two > pr-two.txt; git add pr-two.txt; git commit -qm "pr 2"
        git merge -q --no-ff -m "Merge main into pr (tip)" main
        """
        (self.repo / ".base").write_text(self.base)
        subprocess.run(["bash", "-c", build], cwd=self.repo, check=True, capture_output=True)
        pr_head = self.git("rev-parse", "pr").stdout.strip()
        main_tip = self.git("rev-parse", "main").stdout.strip()
        main_secret = self.git("rev-parse", "main~60").stdout.strip()
        self.assertEqual(
            "main 60 (plants a credential on main)",
            self.git("show", "-s", "--format=%s", main_secret).stdout.strip(),
        )

        def fresh(name):
            clone = Path(self.temp.name) / name
            clone.mkdir()
            def git(*args, check=True):
                return subprocess.run(["git", *args], cwd=clone, text=True,
                                      capture_output=True, check=check)
            git("init", "-q", ".")
            git("config", "gc.auto", "0")
            git("remote", "add", "origin", self.repo.as_uri())
            return clone, git

        def scan(clone, base):
            return subprocess.run(
                [GITLEAKS, "git", f"--log-opts={base}..HEAD", "--redact", "--verbose"],
                cwd=clone, text=True, capture_output=True,
            )

        # Precondition: the truncated graph really does expose main's commit.
        # This is what the gate did before the range-contains-merges unshallow,
        # and it must fail, or the rest of this test proves nothing.
        shallow, git = fresh("shallow")
        git("fetch", "--no-tags", "--prune", "--depth=1", "origin", pr_head)
        git("checkout", "--detach", "--force", pr_head)
        git("fetch", "--no-tags", "--depth=1", "origin", main_tip)
        git("fetch", "--no-tags", "--deepen=50", "origin", pr_head, main_tip)
        self.assertEqual(main_tip, git("merge-base", main_tip, "HEAD").stdout.strip())
        self.assertIn(main_secret,
                      git("rev-list", f"{main_tip}..HEAD").stdout.split())
        leaked = scan(shallow, main_tip)
        self.assertEqual(1, leaked.returncode, leaked.stdout + leaked.stderr)
        self.assertIn("github-pat", leaked.stdout + leaked.stderr)

        for job in ("diff-hygiene", "secret-scan"):
            with self.subTest(job=job):
                clone, git = fresh(f"fixed-{job}")
                result = subprocess.run(
                    ["bash", "-euo", "pipefail", "-c", self.checkout_script(job)],
                    cwd=clone,
                    env={**os.environ, "BASE_SHA": main_tip, "GITHUB_SHA": pr_head,
                         "SCAN_HEAD_SHA": pr_head},
                    text=True, capture_output=True,
                )
                self.assertEqual(0, result.returncode, result.stdout + result.stderr)
                # Merges in the range force a complete graph, so main's own
                # commits are excluded again.
                self.assertEqual(pr_head, git("rev-parse", "HEAD").stdout.strip())
                base = git("merge-base", main_tip, "HEAD").stdout.strip()
                self.assertEqual(main_tip, base)
                clean = scan(clone, base)
                self.assertEqual(0, clean.returncode, clean.stdout + clean.stderr)
                self.assertNotIn(main_secret, git("rev-list", f"{base}..HEAD").stdout.split())
                self.assertEqual("false",
                                 git("rev-parse", "--is-shallow-repository").stdout.strip())

                # A credential the branch itself adds is still reported.
                own = "ghp_" + "Ef5gHi8jKl1mNo4pQr7sTu0vWx3yZa6bCd9E"
                (clone / "branch-secret.txt").write_text(f'github_token = "{own}"\n')
                git("add", "branch-secret.txt")
                git("-c", "user.name=gate", "-c", "user.email=gate@example.invalid",
                    "commit", "-qm", "pr 3 (plants a credential on the branch)")
                planted = scan(clone, base)
                self.assertEqual(1, planted.returncode, planted.stdout + planted.stderr)
                self.assertIn("github-pat", planted.stdout + planted.stderr)


if __name__ == "__main__":
    unittest.main()
