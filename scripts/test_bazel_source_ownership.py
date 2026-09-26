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

    def test_libraries_compile_in_only_declared_package_files(self):
        """A library's non-Rust inputs are its declared embedded assets, no more."""
        policy = generator.SOURCE_OWNERSHIP
        for build in sorted(ROOT.glob("*/*/BUILD.bazel")):
            directory = build.parent.relative_to(ROOT).as_posix()
            declared = expanded(directory, policy.get(directory, {}).get("compile_data", []))
            for args in declarations(directory, {"lash_rust_library", "lash_rust_feature_library"}):
                patterns = ast.literal_eval(args["compile_data_patterns"]) if "compile_data_patterns" in args else []
                files = expanded(directory, patterns)
                self.assertEqual(files, declared, directory)
                self.assertNotIn("Cargo.toml", files, directory)
                self.assertFalse(any(path.startswith("tests/") for path in files), directory)
        postgres = expanded("crates/lash-postgres-store", policy["crates/lash-postgres-store"]["compile_data"])
        self.assertEqual(postgres, {"schema.sql", "teardown.sql", "schema-shape.txt"})

    def test_trybuild_pins_reach_only_the_ui_harness(self):
        directory = "crates/lash"
        pins = expanded(directory, ["tests/ui/*.stderr"])
        self.assertTrue(pins)
        for args in declarations(directory, {"lash_rust_library", "lash_rust_feature_library"}):
            patterns = ast.literal_eval(args["compile_data_patterns"]) if "compile_data_patterns" in args else []
            self.assertFalse(expanded(directory, patterns) & pins)
        functions = {"lash_rust_unit_test", "lash_rust_integration_test", "lash_rust_feature_test"}
        seen = set()
        for args in declarations(directory, functions):
            name = ast.literal_eval(args["crate_name"])
            seen.add(name)
            excluded = expanded(directory, ast.literal_eval(args["data_exclude"])) if "data_exclude" in args else set()
            if name == "ui":
                self.assertFalse(excluded & pins)
            else:
                self.assertTrue(pins <= excluded, name)
        self.assertIn("ui", seen)
        self.assertIn("lash", seen)

    def test_shared_test_data_reaches_each_owner_and_no_other_target(self):
        """A pattern several test targets own is data of each of them."""
        directory = "crates/lash-typescript"
        owners = generator.SOURCE_OWNERSHIP[directory]["test_data"]
        shared = expanded(directory, ["tests/test262/test/**/*.js"])
        self.assertTrue(shared)
        functions = {"lash_rust_unit_test", "lash_rust_integration_test", "lash_rust_feature_test"}
        seen = set()
        for args in declarations(directory, functions):
            name = ast.literal_eval(args["crate_name"])
            seen.add(name)
            excluded = expanded(directory, ast.literal_eval(args["data_exclude"])) if "data_exclude" in args else set()
            if name in owners:
                self.assertFalse(excluded & shared, name)
            else:
                self.assertTrue(shared <= excluded, name)
        self.assertTrue({"test262", "test262_full", "corpus_laws", "integration"} <= seen)

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
            if name.endswith("__fv_866e4b5e"):
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
                generator, "SOURCE_OWNERSHIP", {directory: {"compile_data": patterns}}
            ):
                with self.assertRaises(ValueError):
                    generator.validate_source_ownership(metadata)
        with patch.object(generator, "SOURCE_OWNERSHIP", {directory: {"compile_data": []}}):
            generator.validate_source_ownership(metadata)


if __name__ == "__main__":
    unittest.main()
