#!/usr/bin/env python3
from __future__ import annotations

import pathlib
import subprocess
import tempfile
import textwrap
import unittest


ROOT = pathlib.Path(__file__).resolve().parent.parent
CHECKER = ROOT / "scripts" / "check_feature_coverage.py"


class FeatureCoverageContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tempdir.name)
        (self.root / "member" / "src").mkdir(parents=True)
        (self.root / "scripts").mkdir()
        (self.root / ".github" / "workflows").mkdir(parents=True)
        (self.root / "Cargo.toml").write_text(
            textwrap.dedent(
                """
                [workspace]
                members = ["member"]
                resolver = "3"
                """
            ),
            encoding="utf-8",
        )
        (self.root / "member" / "Cargo.toml").write_text(
            textwrap.dedent(
                """
                [package]
                name = "member"
                version = "0.0.0"
                edition = "2024"

                [features]
                default = []
                testing = []
                """
            ),
            encoding="utf-8",
        )
        (self.root / "member" / "src" / "lib.rs").write_text(
            '#[cfg(feature = "testing")]\npub fn support() {}\n',
            encoding="utf-8",
        )
        self.write_plan()
        (self.root / ".github" / "workflows" / "ci.yml").write_text(
            textwrap.dedent(
                """
                on:
                  merge_group:

                jobs:
                  test-doc:
                    steps:
                      - name: Check workspace (all targets)
                        run: cargo check --workspace --all-targets --locked ${LASH_CI_FEATURES}

                  repo-gates:
                    steps:
                      - name: Test repository scripts
                        run: |
                          python3 scripts/test_check_feature_coverage.py
                          python3 scripts/check_feature_coverage.py check

                  package-feature-checks:
                    if: github.event_name == 'merge_group'
                    strategy:
                      matrix:
                        include:
                          - lane: member-testing
                            command: python3 scripts/check_feature_coverage.py run member-testing
                    steps:
                      - name: Run exact package feature graph
                        run: ${{ matrix.command }}

                  ci-conclusion:
                    needs:
                      - test-doc
                      - package-feature-checks
                """
            ),
            encoding="utf-8",
        )

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def write_plan(self, *, commands: bool = True) -> None:
        command = (
            'commands = [["cargo", "check", "-p", "member", "--lib", '
            '"--no-default-features", "--locked"], '
            '["cargo", "check", "-p", "member", "--lib", '
            '"--no-default-features", "--features", "testing", "--locked"]]'
            if commands
            else "commands = []"
        )
        (self.root / "scripts" / "feature-coverage.toml").write_text(
            textwrap.dedent(
                f"""
                schema = 1

                [baseline]
                command = ["cargo", "check", "--workspace", "--all-targets", "--locked"]
                features = ["member/default"]

                [[lane]]
                name = "member-testing"
                features = ["member/testing:on", "member/testing:off"]
                {command}
                """
            ),
            encoding="utf-8",
        )

    def check(self) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["python3", str(CHECKER), "check", "--root", str(self.root)],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=5,
        )

    def test_complete_contract_passes(self) -> None:
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("2 declared features", result.stdout)

    def test_new_unowned_feature_fails(self) -> None:
        manifest = self.root / "member" / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8") + "unowned = []\n",
            encoding="utf-8",
        )
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unowned declared feature: member/unowned", result.stdout)

    def test_non_cargo_commands_fail(self) -> None:
        plan = self.root / "scripts" / "feature-coverage.toml"
        plan.write_text(
            plan.read_text(encoding="utf-8").replace('"cargo", "check"', '"true"'),
            encoding="utf-8",
        )
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("non-Cargo check/test command", result.stdout)

    def test_all_features_cannot_serve_as_off_witness(self) -> None:
        plan = self.root / "scripts" / "feature-coverage.toml"
        plan.write_text(
            plan.read_text(encoding="utf-8").replace(
                '"--lib", "--no-default-features", "--locked"',
                '"--all-targets", "--all-features", "--no-default-features", "--locked"',
                1,
            ),
            encoding="utf-8",
        )
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("command may not use --all-features", result.stdout)
        self.assertIn("lacks an exact OFF command for member/testing", result.stdout)

    def test_ci_must_execute_the_matrix_command(self) -> None:
        workflow = self.root / ".github" / "workflows" / "ci.yml"
        workflow.write_text(
            workflow.read_text(encoding="utf-8").replace(
                "run: ${{ matrix.command }}", "run: true"
            ),
            encoding="utf-8",
        )
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not execute matrix.command", result.stdout)

    def test_arbitrary_unresolved_feature_fails(self) -> None:
        manifest = self.root / "member" / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8") + "new_gap = []\n",
            encoding="utf-8",
        )
        plan = self.root / "scripts" / "feature-coverage.toml"
        plan.write_text(
            plan.read_text(encoding="utf-8")
            + textwrap.dedent(
                """

                [[unresolved]]
                feature = "member/new_gap"
                reason = "skip"
                tracking = "anything"
                """
            ),
            encoding="utf-8",
        )
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unapproved unresolved feature: member/new_gap", result.stdout)

    def test_baseline_cannot_own_non_default_feature(self) -> None:
        manifest = self.root / "member" / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8") + "new_gap = []\n",
            encoding="utf-8",
        )
        plan = self.root / "scripts" / "feature-coverage.toml"
        plan.write_text(
            plan.read_text(encoding="utf-8").replace(
                'features = ["member/default"]',
                'features = ["member/default", "member/new_gap"]',
            ),
            encoding="utf-8",
        )
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("baseline may own only explicit default features", result.stdout)

    def test_test_conjunction_requires_a_test_context_command(self) -> None:
        manifest = self.root / "member" / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8") + "other = []\n",
            encoding="utf-8",
        )
        source = self.root / "member" / "src" / "lib.rs"
        source.write_text(
            '#[cfg(test)]\nmod tests {\n'
            '    #[cfg(all(feature = "testing", feature = "other"))]\n'
            '    #[test]\n    fn missing() {}\n}\n',
            encoding="utf-8",
        )
        plan = self.root / "scripts" / "feature-coverage.toml"
        contents = plan.read_text(encoding="utf-8")
        contents = contents.replace(
            '"member/testing:on", "member/testing:off"',
            '"member/testing:on", "member/testing:off", "member/other:on", "member/other:off"',
        )
        contents = contents.replace(
            "commands = [",
            'commands = [["cargo", "check", "-p", "member", "--lib", '
            '"--no-default-features", "--features", "other", "--locked"], ',
        )
        plan.write_text(contents, encoding="utf-8")
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lacks a test-context ON command for member/testing", result.stdout)
        self.assertIn("lacks a test-context ON command for member/other", result.stdout)

    def test_dev_self_dependency_cannot_serve_as_off_witness(self) -> None:
        manifest = self.root / "member" / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8")
            + textwrap.dedent(
                """

                [dev-dependencies]
                member = { path = ".", features = ["testing"] }
                """
            ),
            encoding="utf-8",
        )
        plan = self.root / "scripts" / "feature-coverage.toml"
        plan.write_text(
            plan.read_text(encoding="utf-8").replace('"cargo", "check"', '"cargo", "test"'),
            encoding="utf-8",
        )
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lacks an exact OFF command for member/testing", result.stdout)

    def test_target_optional_dependency_feature_is_inventoried(self) -> None:
        manifest = self.root / "member" / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8")
            + textwrap.dedent(
                """

                [target.'cfg(unix)'.dependencies]
                helper = { path = "../helper", optional = true }
                """
            ),
            encoding="utf-8",
        )
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unowned declared feature: member/helper", result.stdout)

    def test_lane_without_an_executable_graph_fails(self) -> None:
        self.write_plan(commands=False)
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lane 'member-testing' has no executable commands", result.stdout)
        self.assertIn("lacks an exact ON command for member/testing", result.stdout)

    def test_workspace_checker_completes_within_ci_budget(self) -> None:
        result = subprocess.run(
            ["python3", str(CHECKER), "check", "--root", str(ROOT)],
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stdout)


if __name__ == "__main__":
    unittest.main()
