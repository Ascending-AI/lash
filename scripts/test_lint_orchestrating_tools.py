#!/usr/bin/env python3
"""Negative fixtures for orchestration discovery and determinism checks."""

from contextlib import redirect_stdout, redirect_stderr
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "lint_orchestrating_tools", Path(__file__).with_name("lint_orchestrating_tools.py")
)
LINT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(LINT)


class OrchestrationLintTests(unittest.TestCase):
    def run_lint(self, body="", *, extra="", prefix="", remove=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for index, relative in enumerate(LINT.EXPECTED_ENTRYPOINTS):
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                name = "execute_orchestration" if index == 0 else "execute_orchestration_by_id"
                path.write_text(prefix + f"\nasync fn {name}() {{\n{body}\n}}\n")
                if remove and index == 0:
                    path.write_text("fn unrelated() {}\n")
            other = root / "crates/new-package/src/lib.rs"
            other.parent.mkdir(parents=True)
            other.write_text(extra)
            output = io.StringIO()
            with patch.object(LINT, "ROOT", root), redirect_stdout(output), redirect_stderr(output):
                status = LINT.main()
            return status, output.getvalue()

    def test_direct_and_aliased_forbidden_calls(self):
        for body, prefix, diagnostic in (
            ("SystemTime::now();", "", "wall clock: SystemTime::now"),
            ("Clock::now();", "use std::time::SystemTime as Clock;", "(aliased import)"),
            ("launch(task);", "use tokio::spawn as launch;", "(aliased import)"),
        ):
            with self.subTest(body=body):
                status, output = self.run_lint(body, prefix=prefix)
                self.assertEqual(status, 1)
                self.assertIn(diagnostic, output)

    def test_comments_strings_and_nested_blocks_are_not_calls(self):
        status, output = self.run_lint(
            '/* outer /* SystemTime::now(); */ comment */\n'
            'let raw = r###"Uuid::new_v4(); }"###;\n'
            'let text = "tokio::spawn(task)";\n'
            'if true { /* HashMap::new(); */ }',
            extra='// async fn execute_orchestration() { rand::random(); }\n'
            'const TEXT: &str = "async fn execute_orchestration() {}";\n',
        )
        self.assertEqual(status, 0, output)

    def test_new_entrypoint_is_discovered_outside_known_paths(self):
        status, output = self.run_lint(extra="async fn execute_orchestration() { rand::random(); }")
        self.assertEqual(status, 1)
        self.assertIn("new-package/src/lib.rs:1: randomness: rand::", output)
        self.assertIn("entrypoint coverage changed", output)

    def test_missing_entrypoint_fails_coverage(self):
        status, output = self.run_lint(remove=True)
        self.assertEqual(status, 1)
        self.assertIn("entrypoint coverage changed", output)

    def test_extra_entrypoint_without_forbidden_calls_still_fails(self):
        status, output = self.run_lint(extra="async\nfn execute_orchestration_by_id () {}")
        self.assertEqual(status, 1)
        self.assertIn("entrypoint coverage changed", output)


if __name__ == "__main__":
    unittest.main()
