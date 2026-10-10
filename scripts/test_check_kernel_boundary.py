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
members = [
    "crates/lash-kernel-doc", "crates/lash-kernel-vm", "crates/lash-ext-regex", "crates/lash-regress",
    "crates/lash-core", "crates/lash-sqlite-store", "crates/lash-dialect-python", "crates/lash",
]

[workspace.dependencies]
serde = "1"
lash-kernel-doc = { path = "crates/lash-kernel-doc" }
lash-regress = { path = "crates/lash-regress" }
lash-core = { package = "lash-internal-core", path = "crates/lash-core" }
lash-kernel-vm = { path = "crates/lash-kernel-vm" }
lash-dialect-python = { path = "crates/lash-dialect-python" }
lash = { path = "crates/lash" }
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
    # A store's tests may drive the facade with a dialect selected.
    "crates/lash-sqlite-store": manifest(
        "lash-internal-sqlite-store",
        "[dependencies]\nlash-core = { workspace = true }\n[dev-dependencies]\nlash = { workspace = true }\n",
    ),
    # A dialect's tests may run what it lowers.
    "crates/lash-dialect-python": manifest(
        "lash-dialect-python",
        "[dependencies]\nlash-kernel-doc = { workspace = true }\n"
        "[dev-dependencies]\nlash-kernel-vm = { workspace = true }\n",
    ),
    "crates/lash": manifest(
        "lash",
        "[dependencies]\nlash-core = { workspace = true }\n"
        "lash-dialect-python = { workspace = true, optional = true }\n",
    ),
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

    def test_a_language_neutral_crate_linking_a_dialect_is_refused(self) -> None:
        cases = {
            # Directly, optional or not.
            "crates/lash-core": manifest(
                "lash-internal-core",
                "[dependencies]\nlash-dialect-python = { workspace = true, optional = true }\n",
            ),
            "crates/lash-kernel-doc": manifest(
                "lash-kernel-doc", "[build-dependencies]\nlash-dialect-python = { workspace = true }\n"
            ),
            # Through another crate's normal dependencies.
            "crates/lash-sqlite-store": manifest(
                "lash-internal-sqlite-store", "[dependencies]\nlash = { workspace = true }\n"
            ),
        }
        for crate, text in cases.items():
            with self.subTest(crate=crate):
                result = self.run_check({crate: text})
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn(f"{crate}/Cargo.toml", result.stderr)
                self.assertIn("reaches the dialect `lash-dialect-python`", result.stderr)

    def test_a_dialect_linking_the_machine_is_refused(self) -> None:
        for table in ("dependencies", "build-dependencies", "target.'cfg(unix)'.dependencies"):
            with self.subTest(table=table):
                result = self.run_check({
                    "crates/lash-dialect-python": manifest(
                        "lash-dialect-python", f"[{table}]\nlash-kernel-vm = {{ workspace = true }}\n"
                    ),
                })
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn("is the machine crate `lash-kernel-vm`", result.stderr)

    def test_this_repository_passes(self) -> None:
        result = subprocess.run(["python3", str(SCRIPT)], capture_output=True, text=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
