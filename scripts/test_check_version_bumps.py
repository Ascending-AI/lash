from __future__ import annotations

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest


SCRIPT = Path(__file__).with_name("check_version_bumps.py")
SPEC = importlib.util.spec_from_file_location("check_version_bumps", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


CONFIG = """
[[surface]]
constant = "WIRE_VERSION"
constant_path = "src/lib.rs"
description = "fixture wire enum"

[[surface.guard]]
kind = "rust_serde_shapes"
paths = ["src/wire.rs"]
"""

LIB_V1 = "pub const WIRE_VERSION: u32 = 1;\n"
LIB_V2 = "pub const WIRE_VERSION: u32 = 2;\n"
WIRE_BASE = """
#[derive(Serialize, Deserialize)]
pub enum WireMessage {
    Existing,
}
"""
WIRE_CHANGED = """
#[derive(Serialize, Deserialize)]
pub enum WireMessage {
    Existing,
    Added,
}
"""


def run(repo: Path, *args: str) -> str:
    result = subprocess.run(
        [*args], cwd=repo, check=True, capture_output=True, text=True
    )
    return result.stdout.strip()


class FixtureRepository:
    def __init__(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        run(self.root, "git", "init", "-q")
        run(self.root, "git", "config", "user.name", "Fixture")
        run(self.root, "git", "config", "user.email", "fixture@example.invalid")
        (self.root / "src").mkdir()
        (self.root / "surface.toml").write_text(
            textwrap.dedent(CONFIG), encoding="utf-8"
        )

    def write(self, version: str, wire: str) -> None:
        (self.root / "src" / "lib.rs").write_text(version, encoding="utf-8")
        (self.root / "src" / "wire.rs").write_text(
            textwrap.dedent(wire), encoding="utf-8"
        )

    def commit(self, message: str) -> str:
        run(self.root, "git", "add", ".")
        run(self.root, "git", "commit", "-q", "-m", message)
        return run(self.root, "git", "rev-parse", "HEAD")

    def close(self) -> None:
        self.temporary.cleanup()


class VersionBumpFixtureTest(unittest.TestCase):
    def fixture(self) -> FixtureRepository:
        fixture = FixtureRepository()
        self.addCleanup(fixture.close)
        return fixture

    def check(self, fixture: FixtureRepository, base: str, head: str):
        surfaces = MODULE.load_config(fixture.root / "surface.toml")
        return MODULE.check_surfaces(fixture.root, base, head, surfaces)

    def test_wire_variant_without_bump_fails(self) -> None:
        fixture = self.fixture()
        fixture.write(LIB_V1, WIRE_BASE)
        base = fixture.commit("base")
        fixture.write(LIB_V1, WIRE_CHANGED)
        head = fixture.commit("add wire variant without bump")

        failures = self.check(fixture, base, head)

        self.assertEqual(len(failures), 1)
        self.assertEqual(failures[0].surface.constant, "WIRE_VERSION")
        self.assertEqual(failures[0].base_version, 1)
        self.assertEqual(failures[0].head_version, 1)

    def test_wire_variant_with_bump_passes(self) -> None:
        fixture = self.fixture()
        fixture.write(LIB_V1, WIRE_BASE)
        base = fixture.commit("base")
        fixture.write(LIB_V2, WIRE_CHANGED)
        head = fixture.commit("add wire variant and bump")

        self.assertEqual(self.check(fixture, base, head), ())

    def test_bump_without_wire_change_passes(self) -> None:
        fixture = self.fixture()
        fixture.write(LIB_V1, WIRE_BASE)
        base = fixture.commit("base")
        fixture.write(LIB_V2, WIRE_BASE)
        head = fixture.commit("reserve next version")

        self.assertEqual(self.check(fixture, base, head), ())

    def test_unrelated_code_in_wire_file_passes(self) -> None:
        fixture = self.fixture()
        fixture.write(LIB_V1, WIRE_BASE + "\nfn helper() -> u8 { 1 }\n")
        base = fixture.commit("base")
        fixture.write(LIB_V1, WIRE_BASE + "\nfn helper() -> u8 { 2 }\n")
        head = fixture.commit("change unrelated helper")

        self.assertEqual(self.check(fixture, base, head), ())


if __name__ == "__main__":
    unittest.main()
