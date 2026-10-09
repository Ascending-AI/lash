#!/usr/bin/env python3
"""Fixture tests for check-kernel-boundary.py."""

from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check-kernel-boundary.py"

WORKSPACE = """\
[workspace]
members = ["crates/lash-kernel-doc", "crates/lash-kernel-vm", "crates/lash-ext-regex", "crates/lash-regress", "crates/lash-core"]

[workspace.dependencies]
serde = "1"
lash-kernel-doc = { path = "crates/lash-kernel-doc" }
lash-regress = { path = "crates/lash-regress" }
lash-core = { package = "lash-internal-core", path = "crates/lash-core" }
"""


def manifest(name: str, body: str = "") -> str:
    return f'[package]\nname = "{name}"\nversion = "0.0.0"\n{body}'


CLEAN = {
    "crates/lash-kernel-doc": manifest("lash-kernel-doc", "[dependencies]\nserde = { workspace = true }\n"),
    "crates/lash-kernel-vm": manifest(
        "lash-kernel-vm",
        "[dependencies]\nlash-kernel-doc = { workspace = true }\n"
        '[dev-dependencies]\nlash-ext-regex = { path = "../lash-ext-regex" }\n',
    ),
    "crates/lash-ext-regex": manifest(
        "lash-ext-regex", '[dependencies]\nregex = "1"\nlash-regress = { workspace = true }\n'
    ),
    # A forked engine is in the set under its own name.
    "crates/lash-regress": manifest("lash-regress", '[dependencies]\nmemchr = "2"\n'),
    # Lash may depend on the kernel; only the reverse is refused.
    "crates/lash-core": manifest("lash-internal-core", "[dependencies]\nlash-kernel-doc = { workspace = true }\n"),
}


class KernelBoundary(unittest.TestCase):
    def run_check(self, overrides: dict[str, str]) -> subprocess.CompletedProcess:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text(WORKSPACE, encoding="utf-8")
            for crate, text in {**CLEAN, **overrides}.items():
                (root / crate).mkdir(parents=True)
                (root / crate / "Cargo.toml").write_text(text, encoding="utf-8")
            return subprocess.run(
                ["python3", str(SCRIPT), str(root)], capture_output=True, text=True, check=False
            )

    def test_the_kernel_set_depending_on_itself_and_third_parties_passes(self) -> None:
        result = self.run_check({})
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_each_dependency_table_and_spelling_is_refused(self) -> None:
        cases = {
            "inherited": "[dependencies]\nlash-core = { workspace = true }\n",
            "by path": '[dependencies]\ncore = { package = "lash-internal-core", path = "../lash-core" }\n',
            "dev": "[dev-dependencies]\nlash-core = { workspace = true }\n",
            "build": "[build-dependencies]\nlash-core = { workspace = true }\n",
            "target": "[target.'cfg(unix)'.dependencies]\nlash-core = { workspace = true }\n",
        }
        for crate in ("lash-kernel-vm", "lash-ext-regex", "lash-regress"):
            for label, body in cases.items():
                with self.subTest(crate=crate, case=label):
                    result = self.run_check({f"crates/{crate}": manifest(crate, body)})
                    self.assertEqual(result.returncode, 1, result.stdout)
                    self.assertIn("lash-internal-core", result.stderr)
                    self.assertIn(f"crates/{crate}/Cargo.toml", result.stderr)

    def test_this_repository_passes(self) -> None:
        result = subprocess.run(["python3", str(SCRIPT)], capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
