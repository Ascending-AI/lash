#!/usr/bin/env python3
"""Compile input boundaries keep their embedded and shared inputs in every lane."""

import ast
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools/bazel"))
import generate_build_files as generator
import feature_variants
sys.path.insert(0, str(ROOT / "scripts"))
import check_feature_coverage as coverage


def declarations(directory, functions):
    tree = ast.parse((ROOT / directory / "BUILD.bazel").read_text())
    for node in tree.body:
        if (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)
                and isinstance(node.value.func, ast.Name)
                and node.value.func.id in functions):
            yield {arg.arg: arg.value for arg in node.value.keywords}


def expanded(directory, patterns):
    package = ROOT / directory
    return {
        path.relative_to(package).as_posix()
        for pattern in patterns for path in package.glob(pattern) if path.is_file()
    }


class SourceOwnershipTests(unittest.TestCase):
    def test_runtime_siblings_keep_shared_and_path_modules_in_every_variant(self):
        directory = "crates/lash-core"
        own = {
            "runtime_turns": "tests/runtime/tests/turns/turn_lifecycle.rs",
            "runtime_observability": "tests/runtime/tests/session_freshness.rs",
            "runtime_lifecycle": "tests/runtime/tests/plugin_lifecycle.rs",
        }
        counts = {name: 0 for name in own}
        for args in declarations(directory, {"lash_rust_integration_test", "lash_rust_feature_test"}):
            name = ast.literal_eval(args["crate_name"])
            if name not in own:
                continue
            counts[name] += 1
            files = expanded(directory, ast.literal_eval(args["srcs_patterns"]))
            self.assertIn(own[name], files)
            self.assertIn("tests/runtime_support/effect_controller_doubles.rs", files)
            for sibling, source in own.items():
                if sibling != name:
                    self.assertNotIn(source, files)
            if name == "runtime_turns":
                self.assertIn("tests/runtime/tests/commit_placement.rs", files)
                self.assertIn("tests/runtime/tests/turn_cancel_modes.rs", files)
        self.assertTrue(all(count > 1 for count in counts.values()), counts)

    def test_library_variants_keep_embedded_schemas_and_lint_config(self):
        for crate in ("lash-core", "lash-core-execution", "lashlang",
                      "lash-postgres-store", "lash-sqlite-store"):
            directory = f"crates/{crate}"
            count = 0
            for args in declarations(directory, {"lash_rust_library", "lash_rust_feature_library"}):
                count += 1
                files = expanded(directory, ast.literal_eval(args["compile_data_patterns"]))
                self.assertIn("Cargo.toml", files)
                self.assertFalse(any(path.startswith("tests/") for path in files))
                self.assertNotIn("src/rendered_sql_pin.txt", files)
                if (ROOT / directory / "clippy.toml").is_file():
                    self.assertIn("clippy.toml", files)
                if crate == "lash-postgres-store":
                    self.assertTrue({"schema.sql", "teardown.sql", "schema-shape.txt"} <= files)
                    self.assertNotIn("payload-schema-fingerprints.txt", files)
            self.assertGreater(count, 1, crate)

    def test_named_feature_tests_keep_both_targets_without_a_unit_harness(self):
        command = feature_variants.parse_command([
            "cargo", "test", "-p", "lash-internal-core-execution",
            "--test", "process_model", "--test", "effect_model", "--no-default-features",
        ])
        self.assertEqual(command.tests, ("process_model", "effect_model"))
        self.assertTrue(command.with_dev)
        selected = {}
        for args in declarations("crates/lash-core-execution", {"lash_rust_feature_test"}):
            name = ast.literal_eval(args["name"])
            if name.endswith("__fv_ecbe9667"):
                selected[ast.literal_eval(args["crate_name"])] = (
                    ast.literal_eval(args["args"]) if "args" in args else []
                )
        self.assertEqual(selected, {"process_model": [], "effect_model": []})

    def test_artifact_census_requires_every_named_integration_test(self):
        package = coverage.Package("sample", ROOT, {}, {}, frozenset())
        command = ["cargo", "test", "-p", "sample", "--test", "first", "--test", "second"]
        def artifact(name):
            return coverage.CargoArtifact(ROOT / "Cargo.toml", name, ("test",),
                                          True, True, frozenset(), frozenset())
        with self.assertRaisesRegex(SystemExit, "second"):
            coverage.validate_selected_artifacts(command, package, [artifact("first")])
        coverage.validate_selected_artifacts(command, package,
                                             [artifact("first"), artifact("second")])

    def test_compile_data_rejects_escape_stale_and_rust_patterns(self):
        directory = "crates/lashlang"
        metadata = {
            "workspace_members": ["lang"],
            "packages": [{
                "id": "lang", "manifest_path": str(ROOT / directory / "Cargo.toml"),
                "targets": [{"name": "lashlang", "kind": ["lib"]}],
            }],
        }
        for patterns in (["../lash-core/Cargo.toml"], ["/tmp/asset"], ["missing.txt"],
                         ["src/lib.rs"], ["src/**"], "Cargo.toml", [1]):
            with self.subTest(patterns=patterns), patch.object(
                generator, "SOURCE_OWNERSHIP", {directory: {"library_compile_data": patterns}}
            ):
                with self.assertRaises(ValueError):
                    generator.validate_source_ownership(metadata)
        with patch.object(generator, "SOURCE_OWNERSHIP", {directory: {"library_compile_data": []}}):
            generator.validate_source_ownership(metadata)


if __name__ == "__main__":
    unittest.main()
