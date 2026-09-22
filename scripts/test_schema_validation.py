from __future__ import annotations

import os
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
EXAMPLE = ROOT / "examples/workflow-graph-roundtrip/scripts/generate-contract-schema.py"


class SchemaValidationTests(unittest.TestCase):
    def test_example_generated_comparison_refuses_missing_and_changed_documents(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            generated = root / "generated"
            checked = root / "checked"
            generated.mkdir()
            checked.mkdir()
            for name in ("workflow-document", "error-response"):
                (generated / f"{name}.schema.json").write_text('{}\n')
                (checked / f"{name}.schema.json").write_text('{}\n')
            stamp = root / "stamp"
            command = [sys.executable, str(EXAMPLE), "--generated", str(generated),
                       "--output", str(checked), "--check", "--stamp", str(stamp)]
            self.assertEqual(subprocess.run(command, capture_output=True).returncode, 0)
            self.assertTrue(stamp.is_file())
            stamp.unlink()
            document = checked / "workflow-document.schema.json"
            document.write_text('changed\n')
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(result.returncode, 1)
            self.assertIn("workflow-document.schema.json", result.stderr)
            self.assertFalse(stamp.exists())
            document.unlink()
            self.assertEqual(subprocess.run(command, capture_output=True).returncode, 1)
            self.assertFalse(stamp.exists())

    def test_example_malformed_generator_json_is_not_a_schema(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            generator = root / "generator"
            generator.write_text("#!/bin/sh\necho not-json\n")
            generator.chmod(0o755)
            result = subprocess.run(
                [sys.executable, str(EXAMPLE), "--generator", str(generator),
                 "--output", str(root / "out")], capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 1)
            self.assertIn("invalid workflow-document schema", result.stderr)
            self.assertFalse((root / "out").exists())

    def test_lint_keeps_off_witness_and_runs_types_after_compile_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / "commands"
            scripts = root / "scripts/ci"
            scripts.mkdir(parents=True)
            for name in ("lint-contracts.sh", "run-gate-commands.sh"):
                shutil.copy2(ROOT / "scripts/ci" / name, scripts / name)
            for name, code in (("cargo", 7), ("npm", 0)):
                tool = root / name
                tool.write_text(f'#!/bin/sh\nprintf "%s\\n" "{name} $*" >> "$COMMAND_LOG"\nexit {code}\n')
                tool.chmod(0o755)
            result = subprocess.run(
                ["bash", str(scripts / "lint-contracts.sh")],
                env={**os.environ, "PATH": f"{root}:{os.environ['PATH']}",
                     "BAZEL_TRUSTED": "true", "COMMAND_LOG": str(log)},
                capture_output=True, text=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("cargo check -p lash-runtime --lib --no-default-features --locked", log.read_text())
            self.assertIn("npm --prefix examples/workflow-graph-roundtrip/frontend run check:generated-types", log.read_text())
            self.assertIn("exit 7", result.stdout)

    def test_portable_ci_route_refuses_trusted_or_unknown_events(self) -> None:
        for decision in ("true", "", "unknown"):
            result = subprocess.run(
                ["bash", str(ROOT / "scripts/ci/check-schema-contracts.sh")],
                env={**os.environ, "BAZEL_TRUSTED": decision}, capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn("only for untrusted CI", result.stderr)


if __name__ == "__main__":
    unittest.main()
