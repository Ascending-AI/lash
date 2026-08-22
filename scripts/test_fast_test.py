"""Self-test for scripts/fast-test.sh.

The value of change-scoped selection rests entirely on the classifier being
fail-closed: a file it silently attributes to the wrong crate, or an input it
treats as crate-local when the whole build graph depends on it, turns a green
run into a false pass. These tests drive the classifier against the *real*
`cargo metadata` of this workspace with synthetic diffs, so a crate rename, a
moved manifest, or a new shared input shows up here rather than as a missed
regression.
"""

from __future__ import annotations

from pathlib import Path
import os
import shutil
import subprocess
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "scripts" / "fast-test.sh"


def classify(*paths: str) -> str:
    result = subprocess.run(
        [str(SCRIPT), "--classify", *paths],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout.strip()


def workspace_packages() -> dict[str, str]:
    """Package name -> repo-relative manifest directory, from real metadata."""
    import json

    raw = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    packages = {}
    for package in json.loads(raw)["packages"]:
        manifest_dir = Path(package["manifest_path"]).resolve().parent
        packages[package["name"]] = str(manifest_dir.relative_to(ROOT))
    return packages


class ClassifierTest(unittest.TestCase):
    def test_script_is_executable(self) -> None:
        self.assertTrue(SCRIPT.exists(), f"{SCRIPT} is missing")
        self.assertTrue(SCRIPT.stat().st_mode & 0o111, f"{SCRIPT} is not executable")

    def test_crate_file_maps_to_its_crate(self) -> None:
        self.assertEqual(classify("crates/lash-trace/src/lib.rs"), "CRATES lash-trace")

    def test_crate_directory_name_is_not_assumed_to_be_the_package_name(self) -> None:
        # crates/lash/ publishes `lash-runtime`. Deriving the package from the
        # directory name would produce a filterset naming a package that does
        # not exist, which nextest rejects — or worse, silently matches nothing.
        packages = workspace_packages()
        self.assertEqual(packages["lash-runtime"], "crates/lash")
        self.assertEqual(classify("crates/lash/src/lib.rs"), "CRATES lash-runtime")

    def test_every_workspace_package_is_reachable_from_a_file_it_owns(self) -> None:
        for name, directory in sorted(workspace_packages().items()):
            with self.subTest(package=name):
                self.assertEqual(
                    classify(f"{directory}/src/synthetic_change.rs"),
                    f"CRATES {name}",
                )

    def test_several_crates_union_and_sort(self) -> None:
        self.assertEqual(
            classify(
                "crates/lash-trace/src/lib.rs",
                "crates/lash-core/src/lib.rs",
                "crates/lash-trace/src/other.rs",
            ),
            "CRATES lash-core lash-trace",
        )

    def test_shared_inputs_fall_back_to_the_full_suite(self) -> None:
        # The asserted *reason* matters, not just the FULL verdict: most of
        # these paths would also land in the unowned-file fallback, so an
        # assertion that only checked for "FULL" would stay green if a shared
        # rule were deleted. A crate-local `Cargo.toml` is the case that proves
        # the rules carry their own weight — without the manifest rule it
        # classifies as its own crate and narrows a dependency change.
        shared = {
            "Cargo.lock": "is a shared build input",
            "Cargo.toml": "is a manifest",
            "crates/lash-core/Cargo.toml": "is a manifest",
            "rust-toolchain.toml": "pins the toolchain",
            ".config/nextest.toml": "is under .config/",
            ".cargo/config.toml": "is under .cargo/",
            "scripts/fast-test.sh": "is under scripts/",
            ".github/workflows/ci.yml": "is under .github/",
        }
        for path, reason in shared.items():
            with self.subTest(path=path):
                verdict = classify(path)
                self.assertEqual(verdict, f"FULL {path} {reason}")

    def test_unowned_file_falls_back_to_the_full_suite(self) -> None:
        for path in ("docs/adr/0045-stateless-substrate.md", "README.md", "CONTEXT.md"):
            with self.subTest(path=path):
                verdict = classify(path)
                self.assertTrue(verdict.startswith("FULL "), verdict)
                self.assertIn("not owned by exactly one workspace crate", verdict)

    def test_empty_diff_falls_back_to_the_full_suite(self) -> None:
        verdict = subprocess.run(
            [str(SCRIPT), "--classify"],
            cwd=ROOT,
            input="",
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
        self.assertEqual(verdict, "FULL the changed-crate set is empty")

    def test_a_moved_files_source_and_destination_both_select(self) -> None:
        # The classifier half of the cross-crate rename hazard: given both ends
        # of a move it must select both crates. Whether git *reports* both ends
        # is pinned by CrossCrateRenameTest below.
        self.assertEqual(
            classify(
                "crates/lash-trace/fixtures/moved.txt",
                "crates/lash-core/fixtures/moved.txt",
            ),
            "CRATES lash-core lash-trace",
        )

    def test_one_shared_input_outvotes_any_number_of_crate_files(self) -> None:
        # Fail-closed means the union is not "crates plus a warning": a single
        # unattributable input widens the run back to everything.
        verdict = classify(
            "crates/lash-trace/src/lib.rs",
            "crates/lash-core/src/lib.rs",
            "Cargo.lock",
        )
        self.assertTrue(verdict.startswith("FULL "), verdict)


class InvocationTest(unittest.TestCase):
    def test_full_fallback_runs_the_unnarrowed_workspace_suite(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn(
            "cargo nextest run --workspace --all-targets --locked)",
            source,
            "the fallback must be the unnarrowed workspace suite",
        )

    def test_zero_selection_is_a_failure_not_a_pass(self) -> None:
        source = SCRIPT.read_text(encoding="utf-8")
        self.assertIn('if [ "$selected_tests" -eq 0 ]; then', source)

    def test_unknown_flag_is_rejected(self) -> None:
        result = subprocess.run(
            [str(SCRIPT), "--nope"],
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("unknown argument", result.stderr)

    def test_a_flag_after_classify_is_rejected_rather_than_classified(self) -> None:
        # Everything after --classify is a path, so `--classify --dry-run` used
        # to report that `--dry-run` is not owned by a crate. Fail loudly.
        result = subprocess.run(
            [str(SCRIPT), "--classify", "--dry-run"],
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("--classify takes paths, not flags", result.stderr)

    def test_the_metadata_temp_file_is_removed_before_every_exec(self) -> None:
        # `exec` replaces the shell image, so the EXIT trap never fires on the
        # two paths that actually run tests; without an explicit cleanup each
        # real run leaks its ~220 KB `cargo metadata` dump.
        lines = [
            line.strip()
            for line in SCRIPT.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        execs = [index for index, line in enumerate(lines) if line.startswith("exec ")]
        self.assertEqual(len(execs), 2, "expected exactly two exec hand-offs")
        for index in execs:
            self.assertEqual(lines[index - 1], "cleanup", lines[index])


class CrossCrateRenameTest(unittest.TestCase):
    """A file moved between crates must select the crate it left, too.

    Under git's default rename detection a move is reported as a single
    destination path, so the source crate loses code and is never selected.
    This drives the real script over a throwaway two-crate workspace, so the
    behavior is pinned end to end rather than by grepping for a flag.
    """

    def _git(self, repo: Path, *args: str) -> str:
        env = dict(os.environ)
        env.update(
            {
                "GIT_AUTHOR_NAME": "fast-test fixture",
                "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
                "GIT_COMMITTER_NAME": "fast-test fixture",
                "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
                "GIT_CONFIG_GLOBAL": str(repo / ".gitconfig-none"),
                "GIT_CONFIG_SYSTEM": str(repo / ".gitconfig-none"),
            }
        )
        return subprocess.run(
            ["git", *args],
            cwd=repo,
            capture_output=True,
            text=True,
            check=True,
            env=env,
        ).stdout

    def _fixture_repo(self, repo: Path) -> None:
        (repo / "scripts").mkdir()
        shutil.copy2(SCRIPT, repo / "scripts" / "fast-test.sh")
        (repo / "Cargo.toml").write_text(
            textwrap.dedent(
                """\
                [workspace]
                resolver = "2"
                members = ["crates/alpha", "crates/beta"]
                """
            ),
            encoding="utf-8",
        )
        for name in ("alpha", "beta"):
            crate = repo / "crates" / name
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                textwrap.dedent(
                    f"""\
                    [package]
                    name = "{name}"
                    version = "0.0.0"
                    edition = "2021"
                    publish = false
                    """
                ),
                encoding="utf-8",
            )
            (crate / "src" / "lib.rs").write_text("", encoding="utf-8")
        # A non-Rust asset: moving one carries no compensating edit to the
        # source crate's `mod`/`lib.rs`, which is exactly when the hole bites.
        (repo / "crates" / "alpha" / "fixture.txt").write_text(
            "shared fixture payload\n" * 8, encoding="utf-8"
        )
        self._git(repo, "init", "--quiet", "-b", "main")
        self._git(repo, "add", "-A")
        self._git(repo, "commit", "--quiet", "-m", "fixture")

    def test_cross_crate_rename_selects_both_crates(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            repo.mkdir()
            self._fixture_repo(repo)
            self._git(
                repo,
                "mv",
                "crates/alpha/fixture.txt",
                "crates/beta/fixture.txt",
            )

            # The hazard is real: with rename detection on, only the
            # destination is reported and `alpha` would vanish from selection.
            detected = self._git(repo, "diff", "--name-only", "HEAD", "--").split()
            self.assertEqual(detected, ["crates/beta/fixture.txt"])

            result = subprocess.run(
                [str(repo / "scripts" / "fast-test.sh"), "--base", "HEAD", "--dry-run"],
                cwd=repo,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("fast-test: selected packages: alpha beta", result.stdout)
            self.assertIn("rdeps(=alpha) + rdeps(=beta)", result.stdout)


if __name__ == "__main__":
    unittest.main()
