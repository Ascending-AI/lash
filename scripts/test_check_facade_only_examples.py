#!/usr/bin/env python3
"""Fixture tests for check_facade_only_examples.py."""

from __future__ import annotations

import contextlib
import io
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import check_facade_only_examples as gate


class FixtureRepository:
    def __init__(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.repo = Path(self.temporary.name)
        self.examples = self.repo / "examples"

    def close(self) -> None:
        self.temporary.cleanup()

    def write(self, relative: str, content: str) -> Path:
        path = self.examples / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        return path

    def package(self, name: str, dependencies: str = "") -> None:
        self.write(
            f"{name}/Cargo.toml",
            f'[package]\nname = "{name}"\nversion = "0.0.0"\n\n'
            f"[dependencies]\n{dependencies}",
        )


class FacadeOnlyExamplesTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = FixtureRepository()
        self.addCleanup(self.fixture.close)

    def violations(self) -> list[tuple[Path, int, str]]:
        with (
            mock.patch.object(gate, "REPO", self.fixture.repo),
            mock.patch.object(gate, "EXAMPLES", self.fixture.examples),
        ):
            return gate.violations()

    def test_seeded_lash_core_import_fails_the_gate(self) -> None:
        self.fixture.package("plain")
        self.fixture.write("plain/src/main.rs", "use lash_core::LashCore;\n")

        with (
            mock.patch.object(gate, "REPO", self.fixture.repo),
            mock.patch.object(gate, "EXAMPLES", self.fixture.examples),
            contextlib.redirect_stderr(io.StringIO()) as errors,
        ):
            status = gate.main()

        self.assertEqual(1, status)
        self.assertIn("examples/plain/src/main.rs:1: lash_core::", errors.getvalue())

    def test_rlm_dependency_does_not_exempt_an_unrelated_source_file(self) -> None:
        self.fixture.package(
            "docs-snippets",
            'lash = { version = "0.1", features = ["rlm"] }\n'
            'lashlang = "0.1"\n',
        )
        self.fixture.write(
            "docs-snippets/src/quickstart.rs", "use lashlang::Program;\n"
        )

        self.assertEqual(
            [(Path("examples/docs-snippets/src/quickstart.rs"), 1, "lashlang::")],
            self.violations(),
        )

    def test_named_rlm_source_is_exempt_only_when_rlm_is_enabled(self) -> None:
        self.fixture.package(
            "docs-snippets",
            'lash = { version = "0.1", features = ["rlm"] }\n'
            'lashlang = "0.1"\n',
        )
        self.fixture.write(
            "docs-snippets/src/embedding_advanced.rs",
            "use lashlang::Program;\n",
        )
        self.assertEqual([], self.violations())

        self.fixture.package("docs-snippets", 'lashlang = "0.1"\n')
        self.assertEqual(
            [
                (
                    Path("examples/docs-snippets/src/embedding_advanced.rs"),
                    1,
                    "lashlang::",
                )
            ],
            self.violations(),
        )

    def test_dev_dependency_rlm_does_not_exempt_named_source(self) -> None:
        self.fixture.write(
            "docs-snippets/Cargo.toml",
            '[package]\nname = "docs-snippets"\nversion = "0.0.0"\n\n'
            "[dev-dependencies]\n"
            'lash = { version = "0.1", features = ["rlm"] }\n'
            'lashlang = "0.1"\n',
        )
        self.fixture.write(
            "docs-snippets/src/embedding_advanced.rs",
            "use lashlang::Program;\n",
        )

        self.assertEqual(
            [
                (
                    Path("examples/docs-snippets/src/embedding_advanced.rs"),
                    1,
                    "lashlang::",
                )
            ],
            self.violations(),
        )

    def test_lashlang_specific_target_remains_exempt(self) -> None:
        self.fixture.package("workflow-graph-roundtrip", 'lashlang = "0.1"\n')
        self.fixture.write(
            "workflow-graph-roundtrip/src/lib.rs",
            "use lashlang::Program;\n",
        )

        self.assertEqual([], self.violations())


if __name__ == "__main__":
    unittest.main()
