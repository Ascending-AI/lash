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


# A synthetic generator set: fixtures name the one stand-in they run.
FIXTURE = ("generator",)


class GenerateWorkflowSchemasTests(unittest.TestCase):
    def setUp(self) -> None:
        # Generation also writes crate-local host contracts. Keep every
        # generated fixture, including those copies, inside the test root.
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = unittest.mock.patch.object(MODULE, "ROOT", Path(directory.name))
        root.start()
        self.addCleanup(root.stop)

    def test_schema_ts_keys_preserve_pattern_map_value_types(self) -> None:
        """SCHEMA-TS-KEYS: typed Document maps keep their value schema in host declarations."""
        schema = {"title": "Document", "type": "object", "additionalProperties": False,
                  "patternProperties": {"^[0-9a-f]{64}$": {"type": "integer"}}}
        declarations = MODULE.typescript_declarations(schema)
        self.assertIn("export type Document = Record<string, number>;", declarations)
        schema["properties"] = {"fixed": {"type": "string"}}
        with self.assertRaises(ValueError):
            MODULE.typescript_declarations(schema)

    def test_trace_tuples_keep_fixed_arity(self) -> None:
        """Trace span tuples must retain their two distinct elements in TypeScript."""
        schema = {"title": "TraceTuple", "type": ["array", "null"],
                  "prefixItems": [{"type": "string"}, {"type": "integer"}],
                  "minItems": 2, "maxItems": 2, "x-lash-schema-version": 37}
        declarations = MODULE.typescript_declarations(schema)
        self.assertIn("export type TraceTuple = ([string, number] | null);", declarations)
        self.assertIn("Derived from v37.schema.json", declarations)
        schema.pop("maxItems")
        with self.assertRaises(ValueError):
            MODULE.typescript_declarations(schema)

    def test_trace_flattened_unions_keep_common_fields(self) -> None:
        """A flattened trace union must retain the context beside its payload."""
        schema = {"title": "TraceUnion", "type": "object",
                  "properties": {"context": {"type": "string"}},
                  "required": ["context"], "unevaluatedProperties": False,
                  "oneOf": [{"type": "object", "properties": {"kind": {"const": "node"}},
                             "required": ["kind"], "additionalProperties": False}]}
        declarations = MODULE.typescript_declarations(schema)
        self.assertIn('"kind": "node";', declarations)
        self.assertIn('"context": string;', declarations)
        self.assertIn(" & ", declarations)
        self.assertNotIn("[key: string]", declarations)

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
            args=[str(Path("generator").resolve())],
            returncode=0,
            stdout='[{"shape":"workflow","version":7,"version_constant":"V","schema":{"type":"object"}},'
            '{"shape":"postgres-host-config","version":1,"schema":{"type":"object"}}]',
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
                self.assertEqual(MODULE.generate(False, [Path("generator")], registered=FIXTURE), 0)
                generated = output / "workflow/v7.schema.json"
                self.assertTrue(generated.is_file())
                for relative in [
                    "crates/lash/host-schemas/workflow/v7.schema.json",
                    "crates/lash/host-schemas/postgres-host-config/v1.schema.json",
                    "crates/lash-postgres-store/postgres-host-config-v1.schema.json",
                ]:
                    crate_copy = MODULE.ROOT / relative
                    expected = crate_copy.read_text(encoding="utf-8")
                    for contents in ["drift\n", None]:
                        with self.subTest(path=relative, contents=contents):
                            if contents is None:
                                crate_copy.unlink()
                            else:
                                crate_copy.write_text(contents, encoding="utf-8")
                            with unittest.mock.patch("sys.stderr", new=io.StringIO()) as errors:
                                self.assertEqual(MODULE.generate(True, [Path("generator")], registered=FIXTURE), 1)
                            self.assertIn(str(crate_copy), errors.getvalue())
                            crate_copy.write_text(expected, encoding="utf-8")
                generated.write_text("drift\n", encoding="utf-8")
                with unittest.mock.patch("sys.stderr", new=io.StringIO()):
                    self.assertEqual(MODULE.generate(True, [Path("generator")], registered=FIXTURE), 1)


    def test_generated_documents_detect_missing_changed_and_obsolete_files(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            generated = root / "generated"
            output = root / "output"
            (generated / "workflow").mkdir(parents=True)
            (generated / "workflow/v7.schema.json").write_text('{"type":"object"}\n')
            with unittest.mock.patch.object(MODULE, "OUTPUT", output):
                with unittest.mock.patch("sys.stderr", new=io.StringIO()):
                    self.assertEqual(MODULE.generate(True, generated=generated), 1)
                self.assertEqual(MODULE.generate(False, generated=generated), 0)
                self.assertEqual(MODULE.generate(True, generated=generated), 0)
                (output / "workflow/v6.schema.json").write_text("obsolete\n")
                with unittest.mock.patch("sys.stderr", new=io.StringIO()) as errors:
                    self.assertEqual(MODULE.generate(True, generated=generated), 1)
                self.assertIn("obsolete", errors.getvalue())
                (output / "workflow/v6.schema.json").unlink()
                (output / "workflow/v7.schema.json").write_text("changed\n")
                with unittest.mock.patch("sys.stderr", new=io.StringIO()) as errors:
                    self.assertEqual(MODULE.generate(True, generated=generated), 1)
                self.assertIn("differs", errors.getvalue())

    def test_workflow_declarations_follow_the_schema_and_refuse_drift(self) -> None:
        schema = {
            "title": "WorkflowView",
            "$defs": {"Site": {"type": "object", "additionalProperties": False,
                               "properties": {"path": {"type": "array", "items": {"type": "integer"}}},
                               "required": ["path"]}},
            "type": "object", "additionalProperties": False,
            "properties": {
                "site": {"$ref": "#/$defs/Site"},
                "parent": {"anyOf": [{"$ref": "#/$defs/Site"}, {"type": "null"}]},
                "kind": {"enum": ["sleep", "perform"]},
                "uses": {"type": "array", "items": {"$ref": "#/$defs/Site"}},
            },
            "required": ["site", "kind", "uses"],
        }
        for shape in ["kernel-workflow-view", "kernel-authoring-operation"]:
            with self.subTest(shape=shape):
                expected = MODULE.rendered_documents([
                    {"shape": shape, "version": 1, "schema": schema}
                ])
                declaration = Path(shape) / "v1.d.ts"
                self.assertIn('export type WorkflowView = {', expected[declaration])
                self.assertIn('"site": Site;', expected[declaration])
                self.assertIn('"parent"?: (Site | null);', expected[declaration])
                self.assertIn('"kind": ("sleep" | "perform");', expected[declaration])
                self.assertIn('"uses": Array<Site>;', expected[declaration])
                self.assertIn('export type Site = {', expected[declaration])
                self.assertIn('"path": Array<number>;', expected[declaration])
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    generated, output = root / "generated", root / "output"
                    for relative, contents in expected.items():
                        path = generated / relative
                        path.parent.mkdir(parents=True, exist_ok=True)
                        path.write_text(contents)
                    with unittest.mock.patch.object(MODULE, "ROOT", root):
                        self.assertEqual(
                            MODULE.generate(False, generated=generated, output=output), 0
                        )
                        for relative, contents in expected.items():
                            self.assertEqual(
                                (root / "crates/lash/host-schemas" / relative).read_text(),
                                contents,
                            )
                        self.assertEqual(MODULE.generate(True, generated=generated, output=output), 0)
                        crate_copies = [root / "crates/lash/host-schemas" / relative
                                        for relative in expected]
                        if shape == "kernel-workflow-view":
                            crate_copies.extend([
                                root / "crates/lash-kernel-view/kernel-workflow-view-v1.schema.json",
                                root / "crates/lash-kernel-view/kernel-workflow-view-v1.d.ts",
                            ])
                        for crate_copy in crate_copies:
                            original = crate_copy.read_text()
                            for contents in ["drift\n", None]:
                                with self.subTest(path=crate_copy, contents=contents):
                                    if contents is None:
                                        crate_copy.unlink()
                                    else:
                                        crate_copy.write_text(contents)
                                    with unittest.mock.patch("sys.stderr", new=io.StringIO()) as errors:
                                        self.assertEqual(MODULE.generate(True, generated=generated, output=output), 1)
                                    self.assertIn(str(crate_copy), errors.getvalue())
                                    crate_copy.write_text(original)
                        (output / declaration).write_text("export type WorkflowView = unknown;\n")
                        with unittest.mock.patch("sys.stderr", new=io.StringIO()) as errors:
                            self.assertEqual(MODULE.generate(True, generated=generated, output=output), 1)
                        self.assertIn("v1.d.ts differs", errors.getvalue())

    def test_generator_failure_does_not_write_a_receipt(self) -> None:
        sys.path.insert(0, str(SCRIPT.parent))
        from host_schema_generators import GENERATORS

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            arguments = []
            for registered in GENERATORS:
                generator = root / registered.binary
                generator.write_text("#!/bin/sh\necho generator-failed >&2\nexit 7\n")
                generator.chmod(0o755)
                arguments.extend(["--generator", str(generator)])
            stamp = root / "stamp"
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--check", *arguments,
                 "--output", str(root / "out"), "--stamp", str(stamp)],
                capture_output=True, text=True,
            )
            self.assertEqual(result.returncode, 7)
            self.assertIn("generator-failed", result.stderr)
            self.assertFalse(stamp.exists())


if __name__ == "__main__":
    unittest.main()
