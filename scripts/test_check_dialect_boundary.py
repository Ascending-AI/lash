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
FIXTURE_TEST = "crates/lash/tests/seam_proof_dialect.rs"
FIXTURE_NAME = "Seam" + "ProofDialect"
FIXTURE_FRONTEND = "Seam" + "ProofFrontend"
FIXTURE_ID = "seam" + "-proof"
FIXTURE_TAG = "<" + "seam>"

CLEAN = {
    ADAPTER: (
        "pub struct TypescriptDialect;\n"
        "impl Dialect for TypescriptDialect {}\n"
        "const OPEN: &str = \"<typescript>\";\n"
        "fn x() { TypescriptDialect::parse; }\n"
    ),
    SHARED: "pub(crate) fn render() -> &'static str { \"shape\" }\n",
    HOST: (
        "use lash::rlm::TypescriptDialect;\n"
        "fn host() { factory(config, Arc::new(TypescriptDialect), &backend); }\n"
    ),
    FIXTURE_TEST: (
        f"struct {FIXTURE_NAME};\n"
        f"const LANGUAGE_ID: &str = \"{FIXTURE_ID}\";\n"
        f"const OPEN: &str = \"{FIXTURE_TAG}\";\n"
    ),
    "crates/lash-protocol-rlm/src/driver.rs": (
        "fn prompt() {}\n"
        "#[cfg(test)]\n"
        "mod tests {\n"
        "    fn t() { let _ = \"<typescript>\"; let _ = TypescriptDialect::default(); }\n"
        "}\n"
    ),
    "docs/adr/0096-typescript-is-the-sole-rlm-dialect.md": (
        "- The law in [the seam proof](../../crates/lash/tests/seam_proof_dialect.rs), "
        f"`{FIXTURE_ID}` included.\n"
    ),
    "docs/adr/0105-the-shift-is-deterministic-workflow-code.md": (
        "The evidence is the seam proof (`lane-seam-proof.report.md`).\n"
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

    def test_the_fixture_dialect_in_a_src_file_fails(self) -> None:
        self.assert_fails(
            {"crates/lash-protocol-rlm/src/testing/mod.rs": f"struct {FIXTURE_NAME};\n"},
            "crates/lash-protocol-rlm/src/testing/mod.rs:1: the seam-proof test dialect",
        )

    def test_the_fixture_frontend_in_a_production_worker_fails(self) -> None:
        self.assert_fails(
            {"crates/lash-vm-worker/src/main.rs": f"worker_entry(&{FIXTURE_FRONTEND});\n"},
            "crates/lash-vm-worker/src/main.rs:1: the seam-proof test dialect",
        )

    def test_the_fixture_frontend_in_its_test_worker_passes(self) -> None:
        result = self.run_check(self.tree({
            "crates/lash/tests/seam_proof_dialect/worker.rs":
                f"worker_entry_with_frontend(&{FIXTURE_FRONTEND});\n",
        }))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_the_fixture_dialect_in_a_docs_page_fails(self) -> None:
        self.assert_fails(
            {"docs/guide.md": f"Try the `{FIXTURE_ID}` dialect with `{FIXTURE_TAG}` cells.\n"},
            "docs/guide.md:1: the seam-proof test dialect",
        )

    def test_an_adr_0096_mention_without_the_evidence_path_fails(self) -> None:
        self.assert_fails(
            {"docs/adr/0096-typescript-is-the-sole-rlm-dialect.md": f"{FIXTURE_NAME} proves it.\n"},
            "docs/adr/0096-typescript-is-the-sole-rlm-dialect.md:1",
        )

    def test_the_concrete_type_carried_outside_the_adapter_fails(self) -> None:
        for line in (
            "struct Driver { dialect: Arc<TypescriptDialect> }",
            "fn render(dialect: &TypescriptDialect) {}",
            "fn parse() { TypescriptDialect::parse_source(\"\"); }",
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
