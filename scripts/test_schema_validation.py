from __future__ import annotations

import importlib.util
import io
import json
import os
import re
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest
import unittest.mock

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
import release_reset  # noqa: E402

WRITER_SPEC = importlib.util.spec_from_file_location(
    "generate_workflow_schemas_routes", ROOT / "scripts/generate-workflow-schemas.py"
)
assert WRITER_SPEC is not None and WRITER_SPEC.loader is not None
WRITER = importlib.util.module_from_spec(WRITER_SPEC)
sys.modules[WRITER_SPEC.name] = WRITER
WRITER_SPEC.loader.exec_module(WRITER)


def workspace_host_generators() -> list[tuple[str, str, str]]:
    """(package, crate directory, binary) of every workspace schema generator.

    Read from the Cargo manifests, independently of the generator catalog,
    so the catalog cannot drop a generator and still agree with itself.
    """
    found = set()
    for manifest in sorted(ROOT.glob("crates/*/Cargo.toml")):
        document = tomllib.loads(manifest.read_text(encoding="utf-8"))
        binaries = {binary["name"] for binary in document.get("bin", [])}
        if document["package"].get("autobins", True):
            binaries.update(path.stem for path in manifest.parent.glob("src/bin/*.rs"))
        found.update(
            (document["package"]["name"], manifest.parent.relative_to(ROOT).as_posix(), binary)
            for binary in binaries
            if binary.endswith("_schema_generator")
        )
    return sorted(found)


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
            shutil.copy2(ROOT / "scripts/host_schema_generators.py", root / "scripts")
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

    def test_every_host_reconciliation_route_enumerates_the_complete_generator_set(self) -> None:
        expected = workspace_host_generators()
        labels = sorted(f"//{directory}:{binary}__bin" for _, directory, binary in expected)
        self.assertTrue(expected)

        root_buck = (ROOT / "BUCK").read_text(encoding="utf-8")
        documents = re.search(
            r'schema_documents\(\s*name = "host_schema_documents",\s*generators = \[(.*?)\]',
            root_buck,
            re.S,
        )
        assert documents is not None
        self.assertEqual(sorted(re.findall(r'"([^"]+)"', documents[1])), labels, "Buck")

        commands: list[list[str]] = []
        with (
            unittest.mock.patch.object(release_reset, "run", lambda _, argv, *__: commands.append(argv)),
            unittest.mock.patch.object(release_reset, "regenerate_fixtures", lambda _: None),
            unittest.mock.patch.object(
                release_reset.subprocess, "check_output", return_value="/out/generator\n"
            ),
        ):
            release_reset.regenerate(ROOT)
        built = next(argv for argv in commands if argv[:2] == ["kiln", "build"])
        self.assertEqual(sorted(arg for arg in built if arg.startswith("//")), labels, "reset")

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shutil.copytree(ROOT / "scripts", root / "scripts")
            log = root / "commands"
            (root / "scripts/generate-workflow-schemas.py").write_text(
                "import os, sys\n"
                "open(os.environ['COMMAND_LOG'], 'a').write('writer ' + ' '.join(sys.argv[1:]) + '\\n')\n"
            )
            cargo = root / "bin/cargo"
            cargo.parent.mkdir()
            cargo.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "args = sys.argv[1:]\n"
                "open(os.environ['COMMAND_LOG'], 'a').write('cargo ' + ' '.join(args) + '\\n')\n"
                "for index, arg in enumerate(args):\n"
                "    if arg == '--bin':\n"
                "        name = args[index + 1]\n"
                "        print(json.dumps({'reason': 'compiler-artifact', 'target': {'name': name},\n"
                "                          'executable': '/out/' + name}))\n"
            )
            cargo.chmod(0o755)
            result = subprocess.run(
                ["bash", str(root / "scripts/ci/check-schema-contracts.sh")],
                env={
                    **os.environ,
                    "PATH": f"{cargo.parent}:{os.environ['PATH']}",
                    "BUCK2_TRUSTED": "false",
                    "COMMAND_LOG": str(log),
                },
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            cargo_line, writer_line = log.read_text().splitlines()
            selected = re.findall(r"-p (\S+) --bin (\S+)", cargo_line)
            self.assertEqual(
                sorted(selected), sorted((package, binary) for package, _, binary in expected), "portable"
            )
            self.assertEqual(
                sorted(re.findall(r"--generator (\S+)", writer_line)),
                sorted(f"/out/{binary}" for _, _, binary in expected),
            )

    def test_an_incomplete_generator_set_changes_no_files(self) -> None:
        binaries = [binary for _, _, binary in workspace_host_generators()]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / "output"
            generators = []
            for index, binary in enumerate(binaries):
                generator = root / "bin" / binary
                generator.parent.mkdir(exist_ok=True)
                generator.write_text(
                    "#!/bin/sh\n"
                    f": > {root}/ran-{binary}\n"
                    f"echo '{json.dumps([{'shape': f'fixture-{index}', 'version': 1, 'schema': {}}])}'\n"
                )
                generator.chmod(0o755)
                generators.append(generator)
            retained = output / "fixture-0/v1.schema.json"
            retained.parent.mkdir(parents=True)
            retained.write_text("retained\n")
            with (
                unittest.mock.patch.object(WRITER, "ROOT", root),
                unittest.mock.patch.object(WRITER, "OUTPUT", output),
                unittest.mock.patch("sys.stderr", new=io.StringIO()) as errors,
            ):
                for check in (False, True):
                    with self.subTest(check=check):
                        self.assertNotEqual(WRITER.generate(check, generators[1:]), 0)
            self.assertIn(binaries[0], errors.getvalue())
            self.assertEqual(
                sorted(path.relative_to(root).as_posix() for path in root.rglob("*")
                       if path.is_file() and path.parent.name != "bin"),
                ["output/fixture-0/v1.schema.json"],
            )
            self.assertEqual(retained.read_text(), "retained\n")

            with (
                unittest.mock.patch.object(WRITER, "ROOT", root),
                unittest.mock.patch.object(WRITER, "OUTPUT", output),
            ):
                self.assertEqual(WRITER.generate(False, generators), 0)
            self.assertEqual(
                sorted(path.parent.name for path in output.glob("*/*.schema.json")),
                sorted(f"fixture-{index}" for index in range(len(binaries))),
            )


if __name__ == "__main__":
    unittest.main()
