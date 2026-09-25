#!/usr/bin/env python3
"""Fixture tests for check_upgrade_paths.py."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import tempfile
import textwrap
import unittest


SCRIPT = Path(__file__).with_name("check_upgrade_paths.py")
SPEC = importlib.util.spec_from_file_location("check_upgrade_paths", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
gate = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = gate
SPEC.loader.exec_module(gate)


REGISTRY = """
[[surface]]
constant = "WIRE_VERSION"
constant_path = "crates/demo/src/lib.rs"
description = "fixture durable format"
manifest = "Wire"

[[surface]]
constant = "JOURNAL_VERSION"
constant_path = "crates/demo/src/journal.rs"
description = "fixture journal"
manifest = "Journal"

[[unregistered]]
constant = "APP_VERSION"
constant_path = "crates/demo/src/lib.rs"
reason = "mirrors the package version"
"""

DECLARATIONS = """
[[surface]]
constant = "WIRE_VERSION"
constant_path = "crates/demo/src/lib.rs"
upgrade = "migrate"

[[surface]]
constant = "JOURNAL_VERSION"
constant_path = "crates/demo/src/journal.rs"
upgrade = "drain"
"""

DECLARED_UNREGISTERED = """
[[surface]]
constant = "APP_VERSION"
constant_path = "crates/demo/src/lib.rs"
upgrade = "coexist"
"""


class CheckTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.dir = Path(self.temp.name)
        self.registry = self.dir / "versioned-surfaces.toml"
        self.declarations = self.dir / "upgrade-paths.toml"
        self.registry.write_text(textwrap.dedent(REGISTRY))

    def declare(self, text: str) -> Path:
        self.declarations.write_text(textwrap.dedent(text))
        return self.declarations

    def problems(self, declarations: str) -> list[str]:
        return gate.check(self.registry, self.declare(declarations))[0]

    def test_complete_declaration_passes(self):
        self.assertEqual(self.problems(DECLARATIONS), [])

    def test_missing_surface_fails(self):
        problems = self.problems(
            """
            [[surface]]
            constant = "WIRE_VERSION"
            constant_path = "crates/demo/src/lib.rs"
            upgrade = "migrate"
            """
        )
        self.assertEqual(len(problems), 1)
        self.assertIn("JOURNAL_VERSION", problems[0])

    def test_stale_entry_fails(self):
        problems = self.problems(
            DECLARATIONS
            + """
            [[surface]]
            constant = "RETIRED_VERSION"
            constant_path = "crates/demo/src/lib.rs"
            upgrade = "drain"
            """
        )
        self.assertEqual(len(problems), 1)
        self.assertIn("RETIRED_VERSION", problems[0])

    def test_unregistered_constant_may_be_declared(self):
        self.assertEqual(self.problems(DECLARATIONS + DECLARED_UNREGISTERED), [])

    def test_duplicate_entry_is_an_error(self):
        with self.assertRaises(gate.DeclarationError):
            self.problems(DECLARATIONS + DECLARATIONS)

    def test_invalid_policy_is_an_error(self):
        with self.assertRaises(gate.DeclarationError):
            self.problems(
                """
                [[surface]]
                constant = "WIRE_VERSION"
                constant_path = "crates/demo/src/lib.rs"
                upgrade = "ignore"
                """
            )

    def test_empty_declaration_file_is_an_error(self):
        with self.assertRaises(gate.DeclarationError):
            self.problems("")

    def test_unreadable_inputs_report_cleanly(self):
        with self.assertRaises(gate.DeclarationError):
            gate.check(self.dir / "absent.toml", self.declarations)

    def test_real_tree_is_consistent(self):
        problems, declared = gate.check(
            Path(__file__).with_name("versioned-surfaces.toml"),
            Path(__file__).with_name("upgrade-paths.toml"),
        )
        self.assertEqual(problems, [])
        self.assertGreaterEqual(len(declared), 1)


if __name__ == "__main__":
    unittest.main()
