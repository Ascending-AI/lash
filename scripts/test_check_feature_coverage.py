#!/usr/bin/env python3
"""Test the feature coverage gate.

When ``ORB_REAL_CARGO`` is set, fixture workspaces resolve Cargo to that real
binary so their single-crate checks do not enter the host's heavy-slot queue.
CI does not set the variable, so its Cargo resolution is unchanged.
"""

from __future__ import annotations

import os
import pathlib
import signal
import subprocess
import tempfile
import textwrap
import time
import unittest


ROOT = pathlib.Path(__file__).resolve().parent.parent
CHECKER = ROOT / "scripts" / "check_feature_coverage.py"
PROCESS_GROUP_GRACE_SECONDS = 0.5


def process_group_exists(process_group: int) -> bool:
    try:
        os.killpg(process_group, 0)
    except ProcessLookupError:
        return False
    return True


def terminate_process_group(process: subprocess.Popen[str]) -> None:
    process_group = process.pid
    try:
        os.killpg(process_group, signal.SIGTERM)
    except ProcessLookupError:
        process.wait()
        return

    deadline = time.monotonic() + PROCESS_GROUP_GRACE_SECONDS
    while time.monotonic() < deadline and process_group_exists(process_group):
        process.poll()
        time.sleep(0.01)
    if process_group_exists(process_group):
        try:
            os.killpg(process_group, signal.SIGKILL)
        except ProcessLookupError:
            pass
    process.wait()


def run_subprocess(
    command: list[str],
    *,
    cwd: pathlib.Path | None = None,
    check: bool = False,
    env: dict[str, str] | None = None,
    timeout: float,
) -> subprocess.CompletedProcess[str]:
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        start_new_session=True,
    )
    try:
        output, _ = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        terminate_process_group(process)
        output, _ = process.communicate()
        raise subprocess.TimeoutExpired(command, timeout, output=output) from None

    completed = subprocess.CompletedProcess(command, process.returncode, output)
    if check:
        completed.check_returncode()
    return completed


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
        self.fixture_env = os.environ.copy()
        if real_cargo := self.fixture_env.get("ORB_REAL_CARGO"):
            if not os.access(real_cargo, os.X_OK):
                self.fail(f"ORB_REAL_CARGO is not executable: {real_cargo}")
            fixture_bin = self.root / ".fixture-bin"
            fixture_bin.mkdir()
            (fixture_bin / "cargo").symlink_to(real_cargo)
            self.fixture_env["PATH"] = os.pathsep.join(
                (str(fixture_bin), self.fixture_env.get("PATH", ""))
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
        return run_subprocess(
            ["python3", str(CHECKER), "check", "--root", str(self.root)],
            check=False,
            timeout=5,
        )

    def run_lane(self) -> subprocess.CompletedProcess[str]:
        run_subprocess(
            ["cargo", "generate-lockfile", "--offline"],
            cwd=self.root,
            check=True,
            env=self.fixture_env,
            timeout=30,
        )
        return run_subprocess(
            [
                "python3",
                str(CHECKER),
                "run",
                "member-testing",
                "--root",
                str(self.root),
            ],
            check=False,
            env=self.fixture_env,
            timeout=30,
        )

    def test_fixture_timeout_terminates_grandchild(self) -> None:
        grandchild_pid = self.root / "grandchild.pid"
        command = [
            "sh",
            "-c",
            'echo timeout-marker; sleep 60 >/dev/null 2>&1 & echo "$!" > "$1"; wait',
            "sh",
            str(grandchild_pid),
        ]
        pid: int | None = None
        try:
            with self.assertRaises(subprocess.TimeoutExpired) as timeout:
                run_subprocess(
                    command,
                    cwd=self.root,
                    check=False,
                    timeout=0.1,
                )
            self.assertIn("timeout-marker", timeout.exception.output)
            pid = int(grandchild_pid.read_text(encoding="utf-8"))
            deadline = time.monotonic() + 0.5
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.01)
            else:
                self.fail(f"grandchild process {pid} survived the fixture timeout")
        finally:
            if pid is not None:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def test_cancelled_checker_terminates_plan_command(self) -> None:
        plan_group_pid = self.root / "plan-group.pid"
        grandchild_pid = self.root / "plan-grandchild.pid"
        runner = self.root / "run-plan-command.py"
        runner.write_text(
            textwrap.dedent(
                f"""
                import pathlib
                import runpy

                checker = runpy.run_path({str(CHECKER)!r})
                checker["run_command"](
                    [
                        "sh",
                        "-c",
                        'echo "$$" > "$1"; sleep 60 & echo "$!" > "$2"; wait',
                        "sh",
                        {str(plan_group_pid)!r},
                        {str(grandchild_pid)!r},
                    ],
                    pathlib.Path({str(self.root)!r}),
                )
                """
            ),
            encoding="utf-8",
        )
        plan_group: int | None = None
        grandchild: int | None = None
        try:
            with self.assertRaises(subprocess.TimeoutExpired):
                run_subprocess(
                    ["python3", str(runner)],
                    cwd=self.root,
                    check=False,
                    timeout=0.5,
                )
            plan_group = int(plan_group_pid.read_text(encoding="utf-8"))
            grandchild = int(grandchild_pid.read_text(encoding="utf-8"))
            self.assertFalse(process_group_exists(plan_group))
            try:
                os.kill(grandchild, 0)
            except ProcessLookupError:
                pass
            else:
                self.fail(f"plan command grandchild {grandchild} survived cancellation")
        finally:
            if plan_group is not None and process_group_exists(plan_group):
                os.killpg(plan_group, signal.SIGKILL)
            if grandchild is not None:
                try:
                    os.kill(grandchild, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def add_other_feature_commands(self, *, combined: bool = False) -> None:
        manifest = self.root / "member" / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8") + "other = []\n",
            encoding="utf-8",
        )
        plan = self.root / "scripts" / "feature-coverage.toml"
        contents = plan.read_text(encoding="utf-8")
        contents = contents.replace(
            '"member/testing:on", "member/testing:off"',
            '"member/testing:on", "member/testing:off", "member/other:on", "member/other:off"',
        )
        commands = (
            'commands = [["cargo", "check", "-p", "member", "--lib", '
            '"--no-default-features", "--features", "other", "--locked"], '
        )
        if combined:
            commands += (
                '["cargo", "check", "-p", "member", "--lib", '
                '"--no-default-features", "--features", "testing,other", "--locked"], '
            )
        plan.write_text(contents.replace("commands = [", commands), encoding="utf-8")

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

    def test_runner_requires_a_whole_conjunction_witness(self) -> None:
        manifest = self.root / "member" / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8") + "other = []\n",
            encoding="utf-8",
        )
        source = self.root / "member" / "src" / "lib.rs"
        source.write_text(
            '#[cfg(all(feature = "testing", feature = "other"))]\n'
            'compile_error!("UNCOVERED_CONJUNCTION");\n',
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

        result = self.run_lane()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no compiled artifact witnesses cfg predicate", result.stdout)
        self.assertIn('all(feature = "testing", feature = "other")', result.stdout)

    def test_runner_composes_enclosing_and_stacked_cfg_predicates(self) -> None:
        self.add_other_feature_commands()
        source = self.root / "member" / "src" / "lib.rs"
        cases = {
            "nested-module-and-impl": textwrap.dedent(
                """
                struct Holder;
                #[cfg(feature = "testing")]
                mod nested {
                    impl super::Holder {
                        #[cfg(feature = "other")]
                        fn support() {}
                    }
                }
                """
            ),
            "stacked-item": textwrap.dedent(
                """
                #[cfg(feature = "testing")]
                #[allow(dead_code)]
                #[cfg(feature = "other")]
                fn support() {}
                """
            ),
        }
        for name, contents in cases.items():
            with self.subTest(name=name):
                source.write_text(contents, encoding="utf-8")
                result = self.run_lane()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("no compiled artifact witnesses cfg predicate true", result.stdout)
                self.assertIn('all(feature = "testing", feature = "other")', result.stdout)

    def test_runner_accepts_combined_effective_predicate_witnesses(self) -> None:
        self.add_other_feature_commands(combined=True)
        source = self.root / "member" / "src" / "lib.rs"
        cases = {
            "nested": (
                '#[cfg(feature = "testing")]\nmod nested {\n'
                '    #[cfg(feature = "other")]\n    pub fn support() {}\n}\n'
            ),
            "stacked": (
                '#[cfg(feature = "testing")]\n#[cfg(feature = "other")]\n'
                'pub fn support() {}\n'
            ),
        }
        for name, contents in cases.items():
            with self.subTest(name=name):
                source.write_text(contents, encoding="utf-8")
                result = self.run_lane()
                self.assertEqual(result.returncode, 0, result.stdout)
                self.assertIn("feature coverage lane passed", result.stdout)

    def test_cfg_attr_behavior_is_witnessed_inside_enclosing_cfg(self) -> None:
        self.add_other_feature_commands()
        source = self.root / "member" / "src" / "lib.rs"
        source.write_text(
            '#[cfg(feature = "other")]\nmod nested {\n'
            '    #[cfg_attr(feature = "testing", allow(dead_code))]\n'
            '    fn support() {}\n}\n',
            encoding="utf-8",
        )

        result = self.run_lane()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no compiled artifact witnesses cfg predicate true", result.stdout)
        self.assertIn('all(feature = "other", feature = "testing")', result.stdout)

    def test_unknown_enclosing_cfg_fails_closed(self) -> None:
        source = self.root / "member" / "src" / "lib.rs"
        source.write_text(
            '#[cfg(custom_build)]\nmod nested {\n'
            '    #[cfg(feature = "testing")]\n    pub fn support() {}\n}\n',
            encoding="utf-8",
        )

        result = self.check()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsupported cfg in feature-gated composition", result.stdout)

    def test_inner_cfg_is_composed_with_nested_item_cfg(self) -> None:
        self.add_other_feature_commands()
        source = self.root / "member" / "src" / "lib.rs"
        source.write_text(
            'mod nested {\n    #![cfg(feature = "testing")]\n'
            '    #[cfg(feature = "other")]\n    pub fn support() {}\n}\n',
            encoding="utf-8",
        )

        result = self.run_lane()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no compiled artifact witnesses cfg predicate true", result.stdout)
        self.assertIn('all(feature = "testing", feature = "other")', result.stdout)

    def test_conditional_cfg_attr_composition_fails_closed(self) -> None:
        source = self.root / "member" / "src" / "lib.rs"
        source.write_text(
            '#[cfg_attr(test, cfg(unix))]\nmod nested {\n'
            '    #[cfg(feature = "testing")]\n    pub fn support() {}\n}\n',
            encoding="utf-8",
        )

        result = self.check()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "conditional cfg_attr in a feature-gated composition is unsupported",
            result.stdout,
        )

    def test_attribute_text_inside_raw_string_is_ignored(self) -> None:
        source = self.root / "member" / "src" / "lib.rs"
        source.write_text(
            'const EXAMPLE: &str = r###"#[cfg(feature = "missing")] {"###;\n'
            '#[cfg(feature = "testing")]\npub fn support() {}\n',
            encoding="utf-8",
        )

        result = self.check()

        self.assertEqual(result.returncode, 0, result.stdout)

    def test_runner_rejects_doctest_as_test_context_artifact(self) -> None:
        source = self.root / "member" / "src" / "lib.rs"
        source.write_text(
            '#[cfg(all(test, feature = "testing"))]\n'
            'compile_error!("UNCOVERED_TEST_TARGET");\n',
            encoding="utf-8",
        )
        plan = self.root / "scripts" / "feature-coverage.toml"
        plan.write_text(
            plan.read_text(encoding="utf-8").replace(
                "commands = [",
                'commands = [["cargo", "test", "-p", "member", "--doc", '
                '"--no-default-features", "--features", "testing", "--locked"], ',
            ),
            encoding="utf-8",
        )

        result = self.run_lane()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lacks a test-context ON command for member/testing", result.stdout)

    def test_runner_rejects_missing_test_artifact(self) -> None:
        manifest = self.root / "member" / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8")
            + "\n[lib]\ntest = false\ndoctest = false\n",
            encoding="utf-8",
        )
        source = self.root / "member" / "src" / "lib.rs"
        source.write_text(
            '#[cfg(all(test, feature = "testing"))]\n'
            'pub fn test_support() {}\n',
            encoding="utf-8",
        )
        plan = self.root / "scripts" / "feature-coverage.toml"
        plan.write_text(
            plan.read_text(encoding="utf-8").replace(
                "commands = [",
                'commands = [["cargo", "test", "-p", "member", "--tests", '
                '"--no-default-features", "--features", "testing", "--locked"], ',
            ),
            encoding="utf-8",
        )

        result = self.run_lane()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("emitted no compiler artifact for selected package member", result.stdout)

    def test_runner_rejects_transitive_dev_feature_as_off_witness(self) -> None:
        manifest = self.root / "member" / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8")
            + textwrap.dedent(
                """
                helper = ["testing"]

                [dev-dependencies]
                member = { path = ".", features = ["helper"] }
                """
            ),
            encoding="utf-8",
        )
        source = self.root / "member" / "src" / "lib.rs"
        source.write_text(
            '#[cfg(test)]\nmod tests {\n'
            '    #[cfg(not(feature = "testing"))]\n'
            '    compile_error!("UNCOVERED_DEV_OFF");\n}\n',
            encoding="utf-8",
        )
        plan = self.root / "scripts" / "feature-coverage.toml"
        contents = plan.read_text(encoding="utf-8")
        contents = contents.replace(
            '"member/testing:on", "member/testing:off"',
            '"member/testing:on", "member/testing:off", "member/helper:on"',
        )
        contents = contents.replace(
            "commands = [",
            'commands = [["cargo", "check", "-p", "member", "--lib", '
            '"--no-default-features", "--features", "helper", "--locked"], '
            '["cargo", "test", "-p", "member", "--no-default-features", "--locked"], ',
        )
        plan.write_text(contents, encoding="utf-8")

        result = self.run_lane()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no compiled artifact witnesses cfg predicate true", result.stdout)
        self.assertIn('not(feature = "testing")', result.stdout)

    def test_runner_accepts_real_test_and_normal_artifacts(self) -> None:
        source = self.root / "member" / "src" / "lib.rs"
        source.write_text(
            '#[cfg(all(test, feature = "testing"))]\n'
            'pub fn test_support() {}\n',
            encoding="utf-8",
        )
        plan = self.root / "scripts" / "feature-coverage.toml"
        plan.write_text(
            plan.read_text(encoding="utf-8").replace(
                "commands = [",
                'commands = [["cargo", "check", "-p", "member", "--tests", '
                '"--no-default-features", "--features", "testing", "--locked"], ',
            ),
            encoding="utf-8",
        )

        result = self.run_lane()

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("feature coverage lane passed", result.stdout)

    def test_runner_preserves_normal_off_when_dev_tests_enable_feature(self) -> None:
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
        source = self.root / "member" / "src" / "lib.rs"
        source.write_text(
            '#[cfg(any(test, feature = "testing"))]\n'
            'pub fn support() {}\n',
            encoding="utf-8",
        )
        plan = self.root / "scripts" / "feature-coverage.toml"
        plan.write_text(
            plan.read_text(encoding="utf-8").replace(
                "commands = [",
                'commands = [["cargo", "test", "-p", "member", '
                '"--no-default-features", "--locked"], ',
            ),
            encoding="utf-8",
        )

        result = self.run_lane()

        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("feature coverage lane passed", result.stdout)

    def test_nested_feature_cfg_attr_fails_closed(self) -> None:
        source = self.root / "member" / "src" / "lib.rs"
        source.write_text(
            '#[cfg_attr(feature = "testing", cfg(feature = "missing"))]\n'
            'pub fn support() {}\n',
            encoding="utf-8",
        )

        result = self.check()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("nested feature-bearing cfg_attr is unsupported", result.stdout)

    def test_unknown_feature_cfg_attr_action_fails_closed(self) -> None:
        source = self.root / "member" / "src" / "lib.rs"
        source.write_text(
            '#[cfg_attr(feature = "testing", path = "alternate.rs")]\n'
            'pub mod support;\n',
            encoding="utf-8",
        )

        result = self.check()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsupported feature-bearing cfg_attr action 'path'", result.stdout)

    def test_cfg_attr_requires_one_top_level_action(self) -> None:
        source = self.root / "member" / "src" / "lib.rs"
        cases = {
            "conditional-cfg-after-allow": (
                '#[cfg_attr(feature = "testing", allow(dead_code), cfg(feature = "other"))]\n'
                'mod nested {\n'
                '    #[cfg(feature = "testing")]\n'
                '    compile_error!("UNCOVERED_MULTI_ACTION_CFG_ATTR");\n'
                '}\n'
            ),
            "conditional-cfg-attr-after-allow": (
                '#[cfg_attr(feature = "testing", allow(dead_code), '
                'cfg_attr(feature = "other", allow(dead_code)))]\n'
                'pub fn support() {}\n'
            ),
            "path-after-allow": (
                '#[cfg_attr(feature = "testing", allow(dead_code), '
                'path = "alternate.rs")]\n'
                'pub mod support;\n'
            ),
        }
        for name, contents in cases.items():
            with self.subTest(name=name):
                source.write_text(contents, encoding="utf-8")
                result = self.check()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(
                    "cfg_attr must contain exactly one action",
                    result.stdout,
                )

    def test_cfg_attr_accepts_one_action_trailing_comma_and_nested_arguments(self) -> None:
        source = self.root / "member" / "src" / "lib.rs"
        cases = {
            "one-action": (
                '#[cfg_attr(feature = "testing", allow(dead_code))]\n'
                'pub fn support() {}\n'
            ),
            "trailing-comma": (
                '#[cfg_attr(feature = "testing", allow(dead_code),)]\n'
                'pub fn support() {}\n'
            ),
            "nested-arguments": (
                '#[cfg_attr(feature = "testing", derive(Clone, Debug))]\n'
                'struct Support;\n'
            ),
        }
        for name, contents in cases.items():
            with self.subTest(name=name):
                source.write_text(contents, encoding="utf-8")
                result = self.check()
                self.assertEqual(result.returncode, 0, result.stdout)

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
        result = run_subprocess(
            ["python3", str(CHECKER), "check", "--root", str(ROOT)],
            check=False,
            timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stdout)


if __name__ == "__main__":
    unittest.main()
