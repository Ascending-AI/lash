#!/usr/bin/env python3
from __future__ import annotations

import ast
import os
import pathlib
import subprocess
import tomllib
import unittest

import yaml


ROOT = pathlib.Path(__file__).resolve().parent.parent


class SlackCloneLiveModelE2eContractTest(unittest.TestCase):
    def test_runner_skips_before_build_when_key_is_absent(self) -> None:
        runner = ROOT / "scripts/slack-clone-live-model-e2e.sh"
        syntax = subprocess.run(
            ["bash", "-n", runner], cwd=ROOT, capture_output=True, text=True, check=False
        )
        self.assertEqual(syntax.returncode, 0, syntax.stderr)
        env = os.environ.copy()
        env.pop("OPENROUTER_API_KEY", None)
        skipped = subprocess.run(
            ["bash", runner], cwd=ROOT, env=env, capture_output=True, text=True, check=False
        )
        self.assertEqual(skipped.returncode, 0, skipped.stderr)
        self.assertIn("SKIP: OPENROUTER_API_KEY is unset", skipped.stdout)
        self.assertNotIn("building", skipped.stdout + skipped.stderr)

    def test_live_binary_is_feature_gated_from_the_standard_example(self) -> None:
        manifest = tomllib.loads(
            (ROOT / "examples/slack-clone/Cargo.toml").read_text(encoding="utf-8")
        )
        self.assertEqual(manifest["features"]["live-e2e"], ["lash/rlm"])
        live_binary = next(
            binary for binary in manifest["bin"] if binary["name"] == "slack-clone-live-e2e"
        )
        self.assertEqual(live_binary["required-features"], ["live-e2e"])

    def test_workflow_is_manual_only_and_uses_the_exact_secret(self) -> None:
        workflow_path = ROOT / ".github/workflows/slack-clone-live-model.yml"
        workflow = yaml.load(workflow_path.read_text(encoding="utf-8"), Loader=yaml.BaseLoader)
        self.assertEqual(workflow["on"], {"workflow_dispatch": ""})
        text = workflow_path.read_text(encoding="utf-8")
        self.assertIn("${{ secrets.OPENROUTER_API_KEY }}", text)
        self.assertIn("if: failure()", text)
        self.assertNotIn("pull_request:", text)
        self.assertNotIn("push:", text)
        self.assertNotIn("schedule:", text)

    def test_driver_has_exact_oracle_and_no_shipped_mutation_switch(self) -> None:
        driver_path = ROOT / "examples/slack-clone/src/live_e2e.rs"
        source = driver_path.read_text(encoding="utf-8")
        tree = ast.parse(
            (ROOT / "scripts/slack-clone-live-model-ui.py").read_text(encoding="utf-8")
        )
        options = {
            node.args[0].value
            for node in ast.walk(tree)
            if isinstance(node, ast.Call)
            and isinstance(node.func, ast.Attribute)
            and node.func.attr == "add_argument"
            and node.args
            and isinstance(node.args[0], ast.Constant)
        }
        self.assertTrue({"--channel-name", "--nonce-a", "--nonce-b"}.issubset(options))
        self.assertIn('snapshot.submissions.get("Agent A") == Some(&expected_b)', source)
        self.assertIn('snapshot.submissions.get("Agent B") == Some(&nonce_a)', source)
        self.assertNotIn("INJECT_NONCE", source)
        self.assertNotIn("--mutation", source)


if __name__ == "__main__":
    unittest.main()
