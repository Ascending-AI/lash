#!/usr/bin/env python3
"""Fixture tests for check-dialect-boundary.py."""

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check-dialect-boundary.py"

ADAPTER = "crates/lash-protocol-rlm/src/dialect/typescript.rs"
SHARED = "crates/lash-protocol-rlm/src/rlm_support.rs"
HOST = "examples/toolbench/src/runtime.rs"

CLEAN = {
    ADAPTER: (
        "pub struct TypescriptPrompts;\n"
        "impl DialectPrompts for TypescriptPrompts {}\n"
        "const OPEN: &str = \"<typescript>\";\n"
        "fn x() { TypescriptPrompts::parse; }\n"
    ),
    SHARED: "pub(crate) fn render() -> &'static str { \"shape\" }\n",
    HOST: (
        "use lash::rlm::TypescriptPrompts;\n"
        "fn host() { factory(config, CellDialect::typescript()); }\n"
    ),
    "crates/lash-protocol-rlm/src/driver.rs": (
        "fn prompt() {}\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    fn t() { let _ = \"<typescript>\"; let _ = TypescriptPrompts::default(); }\n"
        "}\n"
    ),
}


class DialectBoundaryTests(unittest.TestCase):
    def tree(self, overrides: dict[str, str]) -> Path:
        root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, root)
        (root / "scripts").mkdir()
        shutil.copy(SCRIPT, root / "scripts" / SCRIPT.name)
        for path, text in {**CLEAN, **overrides}.items():
            file = root / path
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text(text)
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        subprocess.run(["git", "add", "-A"], cwd=root, check=True)
        return root

    def run_check(self, root: Path) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["python3", str(root / "scripts" / SCRIPT.name)],
            cwd=root,
            text=True,
            capture_output=True,
        )

    def assert_fails(self, overrides: dict[str, str], needle: str) -> None:
        result = self.run_check(self.tree(overrides))
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(needle, result.stderr)

    def test_the_clean_tree_passes(self) -> None:
        result = self.run_check(self.tree({}))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_prompt_text_in_a_cfg_test_module_file_passes(self) -> None:
        result = self.run_check(self.tree({
            "crates/lash-protocol-rlm/src/native/mod.rs": "#[cfg(test)]\nmod cell_reply_laws;\n",
            "crates/lash-protocol-rlm/src/native/cell_reply_laws.rs":
                'const REPLY: &str = "<typescript>print(1)</typescript>";\n',
        }))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_prompt_text_in_a_production_module_file_fails(self) -> None:
        self.assert_fails(
            {
                "crates/lash-protocol-rlm/src/native/mod.rs": "mod cell_reply;\n",
                "crates/lash-protocol-rlm/src/native/cell_reply.rs":
                    'const REPLY: &str = "<typescript>print(1)</typescript>";\n',
            },
            "TypeScript prompt text belongs in",
        )

    def test_the_concrete_type_carried_outside_the_adapter_fails(self) -> None:
        for line in (
            "struct Driver { dialect: Arc<TypescriptPrompts> }",
            "fn render(dialect: &TypescriptPrompts) {}",
            "fn parse() { TypescriptPrompts::parse_source(\"\"); }",
        ):
            with self.subTest(line=line):
                self.assert_fails(
                    {"crates/lash-protocol-rlm/src/protocol/driver.rs": line + "\n"},
                    "may only be named as a value a host selects",
                )

    def test_typescript_prompt_text_in_shared_code_fails(self) -> None:
        self.assert_fails(
            {SHARED: 'fn open() -> &\'static str { "<typescript>" }\n'},
            "TypeScript prompt text belongs in",
        )

    def test_a_retired_binding_name_fails(self) -> None:
        for line in (
            "use lash_core::TYPESCRIPT_TOOL_BINDING_KEY;",
            "let _ = required_tool_typescript_executable(&manifest);",
            'let key = "typescript.tool";',
        ):
            with self.subTest(line=line):
                self.assert_fails({HOST: line + "\n"}, "retired TypeScript binding name")


if __name__ == "__main__":
    unittest.main()
