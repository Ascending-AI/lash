from __future__ import annotations

import importlib.util
import io
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import unittest.mock


SCRIPT = Path(__file__).with_name("generate-workflow-schemas.py")
SPEC = importlib.util.spec_from_file_location("generate_workflow_schemas", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class GenerateWorkflowSchemasTests(unittest.TestCase):
    def test_write_command_targets_the_registered_generator(self) -> None:
        command = MODULE.command()
        self.assertIn("lash-internal-lashlang", command)
        self.assertIn("workflow_schema_generator", command)

    def test_rendered_documents_use_shape_and_version(self) -> None:
        documents = MODULE.rendered_documents(
            [{"shape": "workflow", "version": 7, "schema": {"type": "object"}}]
        )
        self.assertEqual(
            documents,
            {Path("workflow/v7.schema.json"): '{\n  "type": "object"\n}\n'},
        )

    def test_generate_writes_and_check_refuses_drift(self) -> None:
        completed = subprocess.CompletedProcess(
            args=MODULE.command(),
            returncode=0,
            stdout='[{"shape":"workflow","version":7,"version_constant":"V","schema":{"type":"object"}}]',
            stderr="",
        )
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            with (
                unittest.mock.patch.object(MODULE, "OUTPUT", output),
                unittest.mock.patch.object(
                    MODULE.subprocess, "run", return_value=completed
                ),
            ):
                self.assertEqual(MODULE.generate(False), 0)
                generated = output / "workflow/v7.schema.json"
                self.assertTrue(generated.is_file())
                generated.write_text("drift\n", encoding="utf-8")
                with unittest.mock.patch("sys.stderr", new=io.StringIO()):
                    self.assertEqual(MODULE.generate(True), 1)


if __name__ == "__main__":
    unittest.main()
