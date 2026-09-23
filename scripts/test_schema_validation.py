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
    def test_example_generated_comparison_refuses_missing_and_changed_documents(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            generated = root / "generated"
            checked = root / "checked"
            generated.mkdir()
            checked.mkdir()
            for name in ("workflow-document", "error-response"):
                (generated / f"{name}.schema.json").write_text("{}\n")
                (checked / f"{name}.schema.json").write_text("{}\n")
            stamp = root / "stamp"
            command = [
                sys.executable,
                str(EXAMPLE),
                "--generated",
                str(generated),
                "--output",
                str(checked),
                "--check",
                "--stamp",
                str(stamp),
            ]
            self.assertEqual(subprocess.run(command, capture_output=True).returncode, 0)
            self.assertTrue(stamp.is_file())
            stamp.unlink()
            document = checked / "workflow-document.schema.json"
            document.write_text("changed\n")
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
                [
                    sys.executable,
                    str(EXAMPLE),
                    "--generator",
                    str(generator),
                    "--output",
                    str(root / "out"),
                ],
                capture_output=True,
                text=True,
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
                    "BAZEL_TRUSTED": "false",
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
            # The Restate release witness still runs after the OFF witness
            # fails (FIG-3610).
            self.assertIn(
                "cargo check -p lash-runtime --lib --no-default-features "
                "--features restate --locked",
                log.read_text(),
            )
            self.assertIn(
                "npm --prefix examples/workflow-graph-roundtrip/frontend run check:generated-types",
                log.read_text(),
            )
            self.assertIn("exit 7", result.stdout)

    def test_trusted_lint_uses_completed_bazel_contracts(self) -> None:
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
                    "BAZEL_TRUSTED": "true",
                    "COMMAND_LOG": str(log),
                },
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(
                log.read_text().splitlines(),
                ["npm --prefix examples/workflow-graph-roundtrip/frontend run check:generated-types"],
            )

    def test_functional_e2e_checks_schemas_without_kiln_and_keeps_frontend_gates(
        self,
    ) -> None:
        for stale in (False, True):
            with self.subTest(stale=stale), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                for relative in (
                    "scripts/workflow-graph-integration-verify.sh",
                    "scripts/ci/check-schema-contracts.sh",
                    "examples/workflow-graph-roundtrip/scripts/generate-contract-schema.py",
                ):
                    destination = root / relative
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(ROOT / relative, destination)
                frontend = root / "examples/workflow-graph-roundtrip/frontend"
                generated = frontend / "src/generated"
                generated.mkdir(parents=True)
                for shape in ("workflow-document", "error-response"):
                    (generated / f"{shape}.schema.json").write_text("{}\n")
                if stale:
                    (generated / "error-response.schema.json").write_text(
                        '{"stale":true}\n'
                    )
                generator = root / "schema-generator"
                generator.write_text("#!/bin/sh\necho '{}'\n")
                generator.chmod(0o755)
                (root / "scripts/check-workflow-graph-model.sh").write_text(
                    '#!/bin/sh\necho model >> "$COMMAND_LOG"\n'
                )
                bin_dir = root / "bin"
                bin_dir.mkdir()
                cargo = bin_dir / "cargo"
                cargo.write_text(
                    '#!/bin/sh\necho "cargo $*" >> "$COMMAND_LOG"\n'
                    'if [ "$1" = build ]; then\n'
                    '  printf \'{"reason":"compiler-artifact","target":{"name":"workflow_contract_schema"},"executable":"%s"}\\n\' "$SCHEMA_GENERATOR"\n'
                    "fi\n"
                )
                cargo.chmod(0o755)
                for name, code in (("npm", 0), ("kiln", 97)):
                    tool = bin_dir / name
                    tool.write_text(
                        f'#!/bin/sh\necho "{name} $*" >> "$COMMAND_LOG"\nexit {code}\n'
                    )
                    tool.chmod(0o755)
                log = root / "commands"
                result = subprocess.run(
                    [
                        "bash",
                        str(root / "scripts/workflow-graph-integration-verify.sh"),
                    ],
                    env={
                        **os.environ,
                        "PATH": f"{bin_dir}:/usr/bin:/bin",
                        "GITHUB_ACTIONS": "true",
                        "GITHUB_EVENT_NAME": "workflow_dispatch",
                        "BAZEL_TRUSTED": "true",
                        "COMMAND_LOG": str(log),
                        "SCHEMA_GENERATOR": str(generator),
                    },
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(1 if stale else 0, result.returncode, result.stderr)
                commands = log.read_text()
                self.assertNotIn("kiln", commands)
                self.assertEqual(1, commands.count("cargo build"))
                self.assertIn("--bin workflow_contract_schema", commands)
                for gate in (
                    "run check:generated-types",
                    "exec -- vitest run",
                    "exec -- vite build",
                    "cargo test -p workflow-graph-roundtrip --all-targets --locked",
                    "model",
                ):
                    if stale:
                        self.assertNotIn(gate, commands)
                    else:
                        self.assertIn(gate, commands)

    def test_local_workflow_graph_gate_uses_kiln_test_partition(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for relative in (
                "scripts/workflow-graph-integration-verify.sh",
                "examples/workflow-graph-roundtrip/scripts/generate-contract-schema.py",
            ):
                destination = root / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                if relative.endswith("generate-contract-schema.py"):
                    destination.write_text("raise SystemExit(0)\n")
                else:
                    shutil.copy2(ROOT / relative, destination)
            (root / "scripts/check-workflow-graph-model.sh").write_text(
                '#!/bin/sh\necho model >> "$COMMAND_LOG"\n'
            )
            (root / "examples/workflow-graph-roundtrip/frontend").mkdir(parents=True)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            for name in ("npm", "kiln", "cargo"):
                tool = bin_dir / name
                exit_code = 97 if name == "cargo" else 0
                tool.write_text(
                    f'#!/bin/sh\necho "{name} $*" >> "$COMMAND_LOG"\n'
                    f"exit {exit_code}\n"
                )
                tool.chmod(0o755)
            log = root / "commands"
            result = subprocess.run(
                ["bash", str(root / "scripts/workflow-graph-integration-verify.sh")],
                env={
                    **os.environ,
                    "PATH": f"{bin_dir}:/usr/bin:/bin",
                    "GITHUB_ACTIONS": "",
                    "COMMAND_LOG": str(log),
                },
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            commands = log.read_text()
            self.assertIn("kiln test //examples/workflow-graph-roundtrip:test_batch", commands)
            self.assertIn("//examples/workflow-graph-roundtrip:workflow_graph__test", commands)
            self.assertNotIn("cargo ", commands)

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
                env={**os.environ, "BAZEL_TRUSTED": decision},
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn("only for untrusted CI", result.stderr)


if __name__ == "__main__":
    unittest.main()
