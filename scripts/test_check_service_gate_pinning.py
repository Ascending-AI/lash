#!/usr/bin/env python3
"""Tests for scripts/check_service_gate_pinning.py.

Every test here asserts the check *fails* on a defect, because a pinning check
that has only ever been seen to pass proves nothing. The passing cases exist to
show the rule is narrow enough not to fire on the shapes the repository
legitimately uses.
"""

from __future__ import annotations

from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_service_gate_pinning as checker  # noqa: E402


def write_workflow(root: Path, name: str, body: str) -> Path:
    directory = root / ".github" / "workflows"
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / name
    path.write_text(body, encoding="utf-8")
    return path


class RequireFlagRuleTests(unittest.TestCase):
    def setUp(self) -> None:
        self._temp = tempfile.TemporaryDirectory()
        self.root = Path(self._temp.name)
        self.addCleanup(self._temp.cleanup)

    def test_job_env_without_require_flag_fails(self) -> None:
        write_workflow(
            self.root,
            "release.yml",
            """
name: Release
jobs:
  verify:
    runs-on: ubuntu-latest
    env:
      LASH_POSTGRES_DATABASE_URL: postgres://lash:lash@localhost:5432/lash
    steps:
      - run: cargo test -p lash-internal-postgres-store
""",
        )
        violations = checker.check_repository(self.root)
        self.assertEqual(len(violations), 1, violations)
        self.assertIn("LASH_REQUIRE_POSTGRES is unset", violations[0].detail)
        self.assertIn("job `verify` env", violations[0].location)

    def test_job_env_with_require_flag_passes(self) -> None:
        write_workflow(
            self.root,
            "release.yml",
            """
name: Release
jobs:
  verify:
    runs-on: ubuntu-latest
    env:
      LASH_POSTGRES_DATABASE_URL: postgres://lash:lash@localhost:5432/lash
      LASH_REQUIRE_POSTGRES: "1"
    steps:
      - run: cargo test -p lash-internal-postgres-store
""",
        )
        self.assertEqual(checker.check_repository(self.root), [])

    def test_require_flag_set_to_zero_fails(self) -> None:
        write_workflow(
            self.root,
            "ci.yml",
            """
name: CI
jobs:
  verify:
    runs-on: ubuntu-latest
    env:
      LASH_POSTGRES_DATABASE_URL: postgres://lash@localhost/lash
      LASH_REQUIRE_POSTGRES: "0"
    steps:
      - run: cargo test
""",
        )
        violations = checker.check_repository(self.root)
        self.assertEqual(len(violations), 1, violations)
        self.assertIn("LASH_REQUIRE_POSTGRES is '0'", violations[0].detail)

    def test_step_env_inherits_the_flag_from_the_job(self) -> None:
        write_workflow(
            self.root,
            "ci.yml",
            """
name: CI
jobs:
  verify:
    runs-on: ubuntu-latest
    env:
      LASH_REQUIRE_POSTGRES: "1"
    steps:
      - name: Test store
        run: cargo test
        env:
          LASH_POSTGRES_DATABASE_URL: postgres://lash@localhost/lash
""",
        )
        self.assertEqual(checker.check_repository(self.root), [])

    def test_step_env_without_an_enclosing_flag_fails(self) -> None:
        write_workflow(
            self.root,
            "ci.yml",
            """
name: CI
jobs:
  verify:
    runs-on: ubuntu-latest
    steps:
      - name: Test store
        run: cargo test
        env:
          LASH_POSTGRES_DATABASE_URL: postgres://lash@localhost/lash
""",
        )
        violations = checker.check_repository(self.root)
        self.assertEqual(len(violations), 1, violations)
        self.assertIn("`Test store`", violations[0].location)

    def test_minio_endpoint_needs_its_own_flag(self) -> None:
        write_workflow(
            self.root,
            "ci.yml",
            """
name: CI
jobs:
  verify:
    runs-on: ubuntu-latest
    steps:
      - name: S3 conformance
        run: cargo test -p lash-internal-s3-store
        env:
          LASH_MINIO_ENDPOINT: http://127.0.0.1:9000
""",
        )
        violations = checker.check_repository(self.root)
        self.assertEqual(len(violations), 1, violations)
        self.assertIn("LASH_REQUIRE_MINIO is unset", violations[0].detail)


class IgnoredSuiteRuleTests(unittest.TestCase):
    def setUp(self) -> None:
        self._temp = tempfile.TemporaryDirectory()
        self.root = Path(self._temp.name)
        self.addCleanup(self._temp.cleanup)

    def test_nextest_without_run_ignored_fails(self) -> None:
        write_workflow(
            self.root,
            "ci.yml",
            """
name: CI
jobs:
  verify:
    runs-on: ubuntu-latest
    steps:
      - name: Differential
        run: >-
          cargo nextest run -p lash-sim
          --test cross_backend_store_differential
          --locked -j1 --no-capture
""",
        )
        violations = checker.check_repository(self.root)
        self.assertEqual(len(violations), 1, violations)
        self.assertIn("--run-ignored", violations[0].detail)

    def test_nextest_with_run_ignored_passes(self) -> None:
        write_workflow(
            self.root,
            "ci.yml",
            """
name: CI
jobs:
  verify:
    runs-on: ubuntu-latest
    steps:
      - name: Differential
        run: >-
          cargo nextest run -p lash-sim
          --test cross_backend_store_differential
          --locked -j1 --no-capture --run-ignored all
""",
        )
        self.assertEqual(checker.check_repository(self.root), [])

    def test_shell_continuation_is_one_command(self) -> None:
        """The flag on a later continuation line still belongs to the command."""
        scripts = self.root / "scripts"
        scripts.mkdir(parents=True)
        (scripts / "push-gate.sh").write_text(
            'cargo test -p lash-sim \\\n'
            '  --test cross_backend_store_differential \\\n'
            '  --locked -- --nocapture --include-ignored\n',
            encoding="utf-8",
        )
        self.assertEqual(checker.check_repository(self.root), [])

    def test_shell_command_missing_the_flag_fails(self) -> None:
        scripts = self.root / "scripts"
        scripts.mkdir(parents=True)
        (scripts / "push-gate.sh").write_text(
            'cargo test -p lash-sim \\\n'
            '  --test cross_backend_store_differential \\\n'
            '  --locked -- --nocapture\n',
            encoding="utf-8",
        )
        violations = checker.check_repository(self.root)
        self.assertEqual(len(violations), 1, violations)
        self.assertIn("push-gate.sh", violations[0].path)

    def test_a_flag_on_a_neighbouring_command_does_not_satisfy_the_rule(self) -> None:
        """Per-command, not per-file: an unrelated flag must not launder it."""
        scripts = self.root / "scripts"
        scripts.mkdir(parents=True)
        (scripts / "gate.sh").write_text(
            "cargo nextest run --workspace --run-ignored all\n"
            "cargo test -p lash-sim --test cross_backend_store_differential --locked\n",
            encoding="utf-8",
        )
        violations = checker.check_repository(self.root)
        self.assertEqual(len(violations), 1, violations)

    def test_justfile_is_swept(self) -> None:
        (self.root / "justfile").write_text(
            "soak:\n"
            "  cargo test -p lash-sim --test cross_backend_store_differential"
            " generated_cross_backend_surface_differential_agrees -- --nocapture\n",
            encoding="utf-8",
        )
        violations = checker.check_repository(self.root)
        self.assertEqual(len(violations), 1, violations)
        self.assertIn("justfile", violations[0].path)

    def test_run_ignored_default_does_not_satisfy_the_rule(self) -> None:
        """`--run-ignored default` carries the flag and runs no ignored tests."""
        write_workflow(
            self.root,
            "ci.yml",
            """
name: CI
jobs:
  verify:
    runs-on: ubuntu-latest
    steps:
      - name: Differential
        run: >-
          cargo nextest run -p lash-sim
          --test cross_backend_store_differential
          --locked --run-ignored default
""",
        )
        violations = checker.check_repository(self.root)
        self.assertEqual(len(violations), 1, violations)

    def test_run_ignored_ignored_only_satisfies_the_rule(self) -> None:
        write_workflow(
            self.root,
            "ci.yml",
            """
name: CI
jobs:
  verify:
    runs-on: ubuntu-latest
    steps:
      - name: Differential
        run: >-
          cargo nextest run -p lash-sim
          --test cross_backend_store_differential
          --locked --run-ignored ignored-only
""",
        )
        self.assertEqual(checker.check_repository(self.root), [])

    def test_run_ignored_equals_form_is_understood(self) -> None:
        scripts = self.root / "scripts"
        scripts.mkdir(parents=True)
        (scripts / "gate.sh").write_text(
            "cargo nextest run --test cross_backend_store_differential"
            " --run-ignored=all\n",
            encoding="utf-8",
        )
        self.assertEqual(checker.check_repository(self.root), [])

    def test_run_ignored_equals_default_still_fails(self) -> None:
        scripts = self.root / "scripts"
        scripts.mkdir(parents=True)
        (scripts / "gate.sh").write_text(
            "cargo nextest run --test cross_backend_store_differential"
            " --run-ignored=default\n",
            encoding="utf-8",
        )
        self.assertEqual(len(checker.check_repository(self.root)), 1)

    def test_equals_form_of_the_test_selector_is_matched(self) -> None:
        """`--test=<binary>` names the same binary and must not evade the rule."""
        scripts = self.root / "scripts"
        scripts.mkdir(parents=True)
        (scripts / "gate.sh").write_text(
            "cargo test -p lash-sim --test=cross_backend_store_differential --locked\n",
            encoding="utf-8",
        )
        violations = checker.check_repository(self.root)
        self.assertEqual(len(violations), 1, violations)

    def test_equals_form_with_the_opt_in_passes(self) -> None:
        scripts = self.root / "scripts"
        scripts.mkdir(parents=True)
        (scripts / "gate.sh").write_text(
            "cargo test --test=cross_backend_store_differential -- --include-ignored\n",
            encoding="utf-8",
        )
        self.assertEqual(checker.check_repository(self.root), [])

    def test_nested_shell_scripts_are_swept(self) -> None:
        nested = self.root / "scripts" / "gates"
        nested.mkdir(parents=True)
        (nested / "inner.sh").write_text(
            "cargo test --test cross_backend_store_differential --locked\n",
            encoding="utf-8",
        )
        violations = checker.check_repository(self.root)
        self.assertEqual(len(violations), 1, violations)

    def test_yaml_extension_workflows_are_swept(self) -> None:
        write_workflow(
            self.root,
            "extra.yaml",
            """
name: Extra
jobs:
  verify:
    runs-on: ubuntu-latest
    env:
      LASH_POSTGRES_DATABASE_URL: postgres://lash@localhost/lash
    steps:
      - run: cargo test
""",
        )
        violations = checker.check_repository(self.root)
        self.assertEqual(len(violations), 1, violations)

    def test_a_similarly_named_binary_is_not_matched(self) -> None:
        scripts = self.root / "scripts"
        scripts.mkdir(parents=True)
        (scripts / "gate.sh").write_text(
            "cargo test --test cross_backend_store_differential_extra --locked\n",
            encoding="utf-8",
        )
        self.assertEqual(checker.check_repository(self.root), [])

    def test_a_registered_filter_without_the_opt_in_fails(self) -> None:
        """An in-crate live suite is selected by name, not by a test binary."""
        justfile = self.root / "justfile"
        justfile.write_text(
            "cargo test -p lash-internal-restate --locked"
            " live_restate_effect_group_conformance -- --nocapture\n",
            encoding="utf-8",
        )
        violations = checker.check_repository(self.root)
        self.assertEqual(len(violations), 1, violations)
        self.assertIn("live_restate_effect_group_conformance", violations[0].detail)

    def test_a_registered_filter_with_the_ignored_flag_passes(self) -> None:
        justfile = self.root / "justfile"
        justfile.write_text(
            "cargo test -p lash-internal-restate --locked"
            " live_restate_effect_group_conformance -- --ignored --nocapture\n",
            encoding="utf-8",
        )
        self.assertEqual(checker.check_repository(self.root), [])

    def test_the_sdk_precondition_filter_is_registered_too(self) -> None:
        scripts = self.root / "scripts"
        scripts.mkdir(parents=True)
        (scripts / "gate.sh").write_text(
            "cargo test -p lash-internal-restate live_effect_group_sdk_preconditions\n",
            encoding="utf-8",
        )
        self.assertEqual(len(checker.check_repository(self.root)), 1)

    def test_a_prefix_of_a_registered_filter_is_not_matched(self) -> None:
        """The agent-workbench recipe's `live_restate_` prefix is a different run."""
        scripts = self.root / "scripts"
        scripts.mkdir(parents=True)
        (scripts / "gate.sh").write_text(
            "cargo test -p lash-internal-restate live_restate_effect_group\n",
            encoding="utf-8",
        )
        self.assertEqual(checker.check_repository(self.root), [])

    def test_unrelated_test_binaries_are_left_alone(self) -> None:
        scripts = self.root / "scripts"
        scripts.mkdir(parents=True)
        (scripts / "gate.sh").write_text(
            "cargo test -p lash-internal-postgres-store --test conformance --locked\n",
            encoding="utf-8",
        )
        self.assertEqual(checker.check_repository(self.root), [])


class RepositoryTests(unittest.TestCase):
    def test_the_real_repository_passes(self) -> None:
        self.assertEqual(checker.check_repository(checker.ROOT), [])


if __name__ == "__main__":
    unittest.main()
