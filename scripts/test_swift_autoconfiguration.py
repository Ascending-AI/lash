#!/usr/bin/env python3
"""Exercise the applied upstream Swift repository rule without a host compiler."""

import argparse
import ast
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import Mock


def load_rule(source: Path):
    tree = ast.parse(source.read_text())
    tree.body = [node for node in tree.body if not (
        isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)
        and isinstance(node.value.func, ast.Name) and node.value.func.id == "load"
    )]
    namespace = {
        node.id: node.id for node in ast.walk(tree)
        if isinstance(node, ast.Name) and node.id.startswith("SWIFT_")
    }
    namespace["repository_rule"] = lambda **kwargs: kwargs
    exec(compile(tree, str(source), "exec"), namespace)
    return namespace


class SwiftAutoconfigurationTests(unittest.TestCase):
    def test_explicit_opt_out_runs_no_platform_discovery(self):
        namespace = load_rule(SOURCE)
        context = SimpleNamespace(os=SimpleNamespace(environ={
            "BAZEL_DO_NOT_DETECT_SWIFT_TOOLCHAIN": "1",
        }), file=Mock())
        for name in ("_create_xcode_toolchain", "_create_windows_toolchain", "_create_linux_toolchain"):
            namespace[name] = Mock(side_effect=AssertionError("unexpected compiler discovery"))
        namespace["swift_autoconfiguration"]["implementation"](context)
        context.file.assert_called_once()
        self.assertEqual(context.file.call_args.args[0], "BUILD")
        self.assertIn("autoconfiguration was disabled", context.file.call_args.args[1])
        self.assertIn("BAZEL_DO_NOT_DETECT_SWIFT_TOOLCHAIN", namespace["swift_autoconfiguration"]["environ"])

    def test_absent_or_zero_preserves_every_upstream_platform(self):
        for env in ({}, {"BAZEL_DO_NOT_DETECT_SWIFT_TOOLCHAIN": "0"}):
            with self.subTest(env=env):
                namespace = load_rule(SOURCE)
                context = SimpleNamespace(os=SimpleNamespace(environ=env), file=Mock())
                names = ("_create_xcode_toolchain", "_create_windows_toolchain", "_create_linux_toolchain")
                for name in names:
                    namespace[name] = Mock(return_value=name)
                namespace["swift_autoconfiguration"]["implementation"](context)
                for name in names:
                    namespace[name].assert_called_once()
                    self.assertIn(name, context.file.call_args.args[1])


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", required=True, type=Path)
    args, remaining = parser.parse_known_args()
    SOURCE = args.source
    unittest.main(argv=[__file__, *remaining])
