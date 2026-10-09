from __future__ import annotations

import os
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent


class SchemaValidationTests(unittest.TestCase):
    def test_lint_keeps_the_off_witness(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / "commands"
            scripts = root / "scripts/ci"
            scripts.mkdir(parents=True)
            for name in (
                "lint-contracts.sh",
                "run-gate-commands.sh",
                "check-schema-contracts.sh",
            ):
                shutil.copy2(ROOT / "scripts/ci" / name, scripts / name)
            for name, code in (("cargo", 7), ("npm", 0)):
                tool = root / name
                tool.write_text(
                    f'#!/bin/sh\nprintf "%s\\n" "{name} $*" >> "$COMMAND_LOG"\nexit {code}\n'
                )
                tool.chmod(0o755)
            result = subprocess.run(
                ["bash", str(scripts / "lint-contracts.sh")],
                env={
                    **os.environ,
                    "PATH": f"{root}:{os.environ['PATH']}",
                    "BUCK2_TRUSTED": "false",
                    "COMMAND_LOG": str(log),
                },
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(
                "cargo check -p lash-runtime --lib --no-default-features --locked",
                log.read_text(),
            )
            self.assertIn("exit 7", result.stdout)

    def test_trusted_lint_uses_completed_buck2_contracts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / "commands"
            scripts = root / "scripts/ci"
            scripts.mkdir(parents=True)
            for name in ("lint-contracts.sh", "run-gate-commands.sh"):
                shutil.copy2(ROOT / "scripts/ci" / name, scripts / name)
            for name, code in (("cargo", 7), ("npm", 0)):
                tool = root / name
                tool.write_text(
                    f'#!/bin/sh\nprintf "%s\\n" "{name} $*" >> "$COMMAND_LOG"\nexit {code}\n'
                )
                tool.chmod(0o755)
            result = subprocess.run(
                ["bash", str(scripts / "lint-contracts.sh")],
                env={
                    **os.environ,
                    "PATH": f"{root}:{os.environ['PATH']}",
                    "BUCK2_TRUSTED": "true",
                    "COMMAND_LOG": str(log),
                },
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertFalse(log.exists(), "trusted lint runs no portable command")

    def test_functional_e2e_portable_route_requires_explicit_dispatch(self) -> None:
        result = subprocess.run(
            [
                "bash",
                str(ROOT / "scripts/ci/check-schema-contracts.sh"),
                "--functional-e2e",
            ],
            env={
                **os.environ,
                "GITHUB_ACTIONS": "true",
                "GITHUB_EVENT_NAME": "pull_request",
            },
            capture_output=True,
            text=True,
        )
        self.assertEqual(2, result.returncode)
        self.assertIn("explicit GitHub workflow dispatch", result.stderr)

    def test_portable_ci_route_refuses_trusted_or_unknown_events(self) -> None:
        for decision in ("true", "", "unknown"):
            result = subprocess.run(
                ["bash", str(ROOT / "scripts/ci/check-schema-contracts.sh")],
                env={**os.environ, "BUCK2_TRUSTED": decision},
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn("only for untrusted CI", result.stderr)


if __name__ == "__main__":
    unittest.main()
