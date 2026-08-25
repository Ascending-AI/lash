#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import pathlib
import subprocess
import tempfile
import textwrap
import unittest


ROOT = pathlib.Path(__file__).resolve().parent.parent


def load_release_version_module():
    module_path = ROOT / "scripts" / "release_version.py"
    spec = importlib.util.spec_from_file_location("release_version", module_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"failed to load {module_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def write(path: pathlib.Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(text).lstrip())


class ReleaseVersionTest(unittest.TestCase):
    def test_stamp_follows_package_aliases_and_skips_non_lockstep_crates(self) -> None:
        release_version = load_release_version_module()

        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            write(
                root / "Cargo.toml",
                """
                [workspace]
                members = ["crates/public-crate", "runbooks/private-crate"]
                resolver = "3"

                [workspace.package]
                version = "0.1.0-alpha.1"
                edition = "2024"

                [workspace.dependencies]
                public-crate = { package = "lash-internal-public-crate", path = "crates/public-crate", version = "=0.1.0-alpha.1" }
                """,
            )
            write(
                root / "crates/public-crate/Cargo.toml",
                """
                [package]
                name = "lash-internal-public-crate"
                version.workspace = true
                edition.workspace = true
                """,
            )
            write(root / "crates/public-crate/src/lib.rs", "")
            write(
                root / "runbooks/private-crate/Cargo.toml",
                """
                [package]
                name = "private-crate"
                version = "0.0.0"
                edition.workspace = true
                publish = false

                [dependencies]
                public-crate = { workspace = true }
                """,
            )
            write(root / "runbooks/private-crate/src/lib.rs", "")
            for doc_path in [
                root / "README.md",
                root / "docs/index.html",
                root / "docs/quickstart.html",
                root / "docs/tracing.html",
            ]:
                write(doc_path, "\n")
            write(root / "docs/released-version.txt", "0.1.0-alpha.1\n")

            subprocess.run(
                ["cargo", "metadata", "--format-version", "1"],
                cwd=root,
                check=True,
                stdout=subprocess.DEVNULL,
            )

            original_root = release_version.ROOT
            original_cargo_toml = release_version.CARGO_TOML
            original_cargo_lock = release_version.CARGO_LOCK
            original_doc_version_files = release_version.DOC_VERSION_FILES
            original_released_version_file = release_version.RELEASED_VERSION_FILE
            try:
                release_version.ROOT = root
                release_version.CARGO_TOML = root / "Cargo.toml"
                release_version.CARGO_LOCK = root / "Cargo.lock"
                release_version.DOC_VERSION_FILES = [
                    root / "README.md",
                    root / "docs/index.html",
                    root / "docs/quickstart.html",
                    root / "docs/tracing.html",
                ]
                release_version.RELEASED_VERSION_FILE = root / "docs/released-version.txt"
                release_version.apply_version_text("0.1.0-alpha.2")
            finally:
                release_version.ROOT = original_root
                release_version.CARGO_TOML = original_cargo_toml
                release_version.CARGO_LOCK = original_cargo_lock
                release_version.DOC_VERSION_FILES = original_doc_version_files
                release_version.RELEASED_VERSION_FILE = original_released_version_file

            subprocess.run(
                ["cargo", "metadata", "--format-version", "1", "--locked"],
                cwd=root,
                check=True,
                stdout=subprocess.DEVNULL,
            )

            lockfile = (root / "Cargo.lock").read_text()
            self.assertIn(
                'name = "lash-internal-public-crate"\nversion = "0.1.0-alpha.2"',
                lockfile,
            )
            self.assertIn('name = "private-crate"\nversion = "0.0.0"', lockfile)
            self.assertNotIn('name = "private-crate"\nversion = "0.1.0-alpha.2"', lockfile)
            self.assertIn(
                'public-crate = { package = "lash-internal-public-crate", '
                'path = "crates/public-crate", version = "=0.1.0-alpha.2" }',
                (root / "Cargo.toml").read_text(),
            )
            self.assertEqual(
                (root / "docs/released-version.txt").read_text(),
                "0.1.0-alpha.2\n",
            )


if __name__ == "__main__":
    unittest.main()
