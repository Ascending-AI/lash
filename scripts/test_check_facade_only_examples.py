#!/usr/bin/env python3
"""Fixture tests for the facade-only consumer gate."""

from __future__ import annotations

import contextlib
import io
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import check_facade_only_examples as gate


class FacadeOnlyExamplesTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.repo = Path(temporary.name)
        self.write(
            "crates/lash/Cargo.toml",
            '[dependencies]\nlash-core = "0.1"\nlash-durable = "0.1"\n'
            'lashlang = "0.1"\n',
        )
        patch = mock.patch.object(gate, "REPO", self.repo)
        patch.start()
        self.addCleanup(patch.stop)

    def write(self, relative: str, content: str) -> Path:
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        return path

    def test_seeded_lash_core_import_fails_the_gate(self) -> None:
        self.write("examples/plain/src/main.rs", "use lash_core::LashCore;\n")
        with contextlib.redirect_stderr(io.StringIO()) as errors:
            status = gate.main()
        self.assertEqual(1, status)
        self.assertIn("examples/plain/src/main.rs:1: lash_core::", errors.getvalue())

    def test_seeded_durable_import_fails_the_gate(self) -> None:
        self.write("examples/plain/src/main.rs", "use lash_durable::DurableBackend;\n")
        self.assertEqual(
            [(Path("examples/plain/src/main.rs"), 1, "lash_durable::")],
            gate.violations(),
        )

    def test_new_facade_dependencies_and_transitive_crates_are_forbidden(self) -> None:
        self.write("crates/lash/Cargo.toml", '[dependencies]\nlash-new-backend = "0.1"\n')
        self.write(
            "crates/lash-new-backend/Cargo.toml",
            '[dependencies]\nlash-backend-support = "0.1"\n',
        )
        self.write(
            "examples/plain/src/main.rs",
            "use lash_new_backend::Backend;\nuse lash_backend_support::State;\n",
        )
        self.assertEqual(
            [
                (Path("examples/plain/src/main.rs"), 1, "lash_new_backend::"),
                (Path("examples/plain/src/main.rs"), 2, "lash_backend_support::"),
            ],
            gate.violations(),
        )

    def test_rlm_has_no_lashlang_source_exemptions(self) -> None:
        self.write(
            "examples/agent-workbench/Cargo.toml",
            '[dependencies]\nlash = { version = "0.1", features = ["rlm"] }\n',
        )
        self.write("examples/agent-workbench/src/turns.rs", "use lashlang::Program;\n")
        self.assertEqual(
            [(Path("examples/agent-workbench/src/turns.rs"), 1, "lashlang::")],
            gate.violations(),
        )

    def test_seeded_runbook_host_import_fails_the_gate(self) -> None:
        self.write("runbooks/rlm-smoke/src/main.rs", "use lash_core::LashCore;\n")
        self.assertEqual(
            [(Path("runbooks/rlm-smoke/src/main.rs"), 1, "lash_core::")],
            gate.violations(),
        )

    def test_dependency_alias_and_crate_rename_cannot_bypass_the_gate(self) -> None:
        self.write(
            "examples/plain/Cargo.toml",
            '[dependencies]\nengine = { package = "lash-internal-durable", version = "0.1" }\n',
        )
        self.write(
            "examples/plain/src/main.rs",
            "use engine::DurableBackend;\nuse lash_core as core;\nextern crate lashlang;\n",
        )
        self.assertEqual(
            [
                (Path("examples/plain/src/main.rs"), 1, "engine::"),
                (Path("examples/plain/src/main.rs"), 2, "lash_core"),
                (Path("examples/plain/src/main.rs"), 3, "lashlang"),
            ],
            gate.violations(),
        )

    def test_facade_paths_and_independent_test_tooling_are_allowed(self) -> None:
        self.write(
            "examples/plain/src/main.rs",
            "use lash::durable::DurableBackend;\n"
            "#[cfg(test)]\nmod tests { use lash_durable_test::DurableTestBackend; }\n",
        )
        self.write("runbooks/rlm-smoke/src/main.rs", "use lash::LashCore;\n")
        self.assertEqual([], gate.violations())


if __name__ == "__main__":
    unittest.main()
