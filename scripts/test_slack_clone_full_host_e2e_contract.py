#!/usr/bin/env python3
from __future__ import annotations

import ast
import pathlib
import subprocess
import tomllib
import unittest


ROOT = pathlib.Path(__file__).resolve().parent.parent


class SlackCloneFullHostE2eContractTest(unittest.TestCase):
    def test_runner_rejects_arguments_before_starting_services(self) -> None:
        runner = ROOT / "scripts/slack-clone-full-host-e2e.sh"
        syntax = subprocess.run(
            ["bash", "-n", runner],
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(syntax.returncode, 0, syntax.stderr)

        rejected = subprocess.run(
            ["bash", runner, "unexpected"],
            cwd=ROOT,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(rejected.returncode, 2)
        self.assertIn("usage:", rejected.stderr)

    def test_driver_has_four_machine_gates_and_no_shipped_mutation_mode(self) -> None:
        driver_path = ROOT / "scripts/slack-clone-full-host-e2e.py"
        tree = ast.parse(driver_path.read_text(encoding="utf-8"), filename=str(driver_path))

        layers: tuple[str, ...] | None = None
        gate_layers: set[str] = set()
        parser_options: set[str] = set()
        methods: set[str] = set()
        for node in ast.walk(tree):
            if isinstance(node, ast.Assign):
                for target in node.targets:
                    if isinstance(target, ast.Name) and target.id == "LAYERS":
                        value = ast.literal_eval(node.value)
                        self.assertIsInstance(value, tuple)
                        layers = value
            elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                methods.add(node.name)
            elif isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
                if node.func.attr == "gate" and len(node.args) >= 2:
                    layer = node.args[1]
                    if isinstance(layer, ast.Constant) and isinstance(layer.value, str):
                        gate_layers.add(layer.value)
                if node.func.attr == "add_argument" and node.args:
                    option = node.args[0]
                    if isinstance(option, ast.Constant) and isinstance(option.value, str):
                        parser_options.add(option.value)

        self.assertEqual(layers, ("dom", "platform", "bot", "trace"))
        self.assertEqual(gate_layers, set(layers))
        self.assertNotIn("--mutation", parser_options)
        self.assertTrue(
            {
                "checkpoint_ambient",
                "checkpoint_room_mention",
                "checkpoint_thread",
                "checkpoint_redelivery",
                "checkpoint_kill_restart",
                "checkpoint_mcp_depth",
                "checkpoint_reload",
            }.issubset(methods)
        )

    def test_e2e_provider_is_feature_gated(self) -> None:
        manifest = tomllib.loads(
            (ROOT / "examples/slack-clone/Cargo.toml").read_text(encoding="utf-8")
        )
        self.assertEqual(manifest["features"]["e2e"], ["lash/testing"])


if __name__ == "__main__":
    unittest.main()
