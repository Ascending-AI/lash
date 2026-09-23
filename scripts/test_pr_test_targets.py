#!/usr/bin/env python3
"""Policy checks for the conservative pull-request Bazel selector."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "pr_test_targets", ROOT / "scripts/ci/pr_test_targets.py"
)
assert SPEC is not None and SPEC.loader is not None
selector = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(selector)


class PrTestTargetTests(unittest.TestCase):
    def test_core_change_includes_conformance_reverse_dependency(self) -> None:
        allowed, batches = selector.inventory()
        downstream = "//crates/lash-conformance:lash-conformance__unit_test"
        self.assertIn(downstream, allowed)
        selected = selector.select(
            ["crates/lash-core/src/lib.rs"], downstream + "\n", allowed, batches
        )
        self.assertIn(downstream, selected)
        self.assertNotEqual(selector.FULL_SUITE, selected)

    def test_batch_compacts_only_when_all_members_are_selected(self) -> None:
        allowed, batches = selector.inventory()
        batch = "//crates/lash-core-execution:test_batch"
        members = set(batches[batch])
        self.assertTrue(members <= allowed)
        self.assertEqual([batch], selector.compact(members, batches))
        partial = members - {next(iter(members))}
        self.assertNotIn(batch, selector.compact(partial, batches))

    def test_unknown_or_global_inputs_take_full_suite(self) -> None:
        for paths in (
            ["Cargo.lock"],
            [".github/workflows/ci.yml"],
            ["tools/bazel/generate_build_files.py"],
            ["crates/lash-core/Cargo.toml"],
            ["crates/lash-core/tests/fixture.json"],
            ["crates/lash-typescript/README.md", "crates/lash-core/src/lib.rs"],
            ["crates/nonexistent/src/lib.rs"],
        ):
            with self.subTest(paths=paths):
                self.assertIsNone(selector.packages_for(paths))

    def test_empty_or_unrecognized_query_cannot_drop_test_proof(self) -> None:
        allowed, batches = selector.inventory()
        paths = ["crates/lash-core/src/lib.rs"]
        self.assertEqual(selector.FULL_SUITE, selector.select(paths, "", allowed, batches))
        self.assertEqual(
            selector.FULL_SUITE,
            selector.select(paths, "unexpected query output\n", allowed, batches),
        )

    def test_deleted_path_or_unrelated_base_falls_back(self) -> None:
        def result(code: int, output: bytes = b"") -> subprocess.CompletedProcess[bytes]:
            return subprocess.CompletedProcess(["git"], code, output, b"")

        with mock.patch.object(selector, "git", side_effect=[
            result(0), result(0, b"D\0crates/lash-core/src/lib.rs\0"), result(0),
        ]):
            self.assertIsNone(selector.changed_paths("base"))
        with mock.patch.object(selector, "git", side_effect=[
            result(0), result(0, b"M\0crates/lash-core/src/lib.rs\0"), result(1),
        ]):
            self.assertIsNone(selector.changed_paths("base"))


if __name__ == "__main__":
    unittest.main()
