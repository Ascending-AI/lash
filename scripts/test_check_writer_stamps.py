#!/usr/bin/env python3
"""Fixture tests for check_writer_stamps.py."""

from __future__ import annotations

import contextlib
import importlib.util
import io
from pathlib import Path
import sys
import tempfile
import textwrap
import unittest


SCRIPT = Path(__file__).with_name("check_writer_stamps.py")
SPEC = importlib.util.spec_from_file_location("check_writer_stamps", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
gate = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = gate
SPEC.loader.exec_module(gate)


CONSTANT = "WIDGET_FORMAT_VERSION"

DIRECT_STAMP = """\
pub struct Row {
    version: u32,
}

fn write() -> Row {
    Row { version: WIDGET_FORMAT_VERSION as u32 }
}
"""

ROUTED_STAMP = """\
fn write(fleet: FleetFormat) -> Row {
    Row { version: fleet.writer_version(surface_format!(WIDGET_FORMAT_VERSION)) }
}
"""

MANIFEST_ROW = """\
static DURABLE_FORMATS: &[EngineDurableFormat] = &[
    EngineDurableFormat {
        id: "durable.widget",
        name: "Durable widget",
        version: WIDGET_FORMAT_VERSION as u32,
        constant: "WIDGET_FORMAT_VERSION",
        upgrade_policy: UpgradePolicy::Migrate,
        unwalkable_reason: REASON,
    },
];
"""


class CheckTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)

    def write(self, relative: str, text: str) -> None:
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(text), encoding="utf-8")

    def check(self) -> list[str]:
        return gate.check(gate.WorktreeSources(self.repo), [CONSTANT])

    def command(self) -> tuple[int, str]:
        errors = io.StringIO()
        with contextlib.redirect_stderr(errors), contextlib.redirect_stdout(io.StringIO()):
            status = gate.main(["--repo", str(self.repo)])
        return status, errors.getvalue()

    def declare(self, surface: str = "migrate") -> None:
        # Source declaration is the only inventory: the registry beside it
        # carries exclusions and no surface rows.
        self.write("scripts/versioned-surfaces.toml", "# exclusions only\n")
        self.write(
            "crates/widget-store/src/version.rs",
            f"""\
            /// version_surface = "{surface}"
            pub const {CONSTANT}: u32 = 1;
            """,
        )

    def test_direct_write_stamp_is_flagged(self):
        self.write("crates/widget-store/src/lib.rs", DIRECT_STAMP)
        failures = self.check()
        self.assertEqual(len(failures), 1)
        self.assertIn("crates/widget-store/src/lib.rs:6", failures[0])
        self.assertIn("FleetFormat::writer_version", failures[0])

    def test_fleet_routed_stamp_is_not_flagged(self):
        self.write("crates/widget-store/src/lib.rs", ROUTED_STAMP)
        self.assertEqual(self.check(), [])

    def test_manifest_exemption_does_not_leak_to_other_paths(self):
        self.write("crates/widget-store/src/lib.rs", MANIFEST_ROW)
        failures = self.check()
        self.assertEqual(len(failures), 1)
        self.assertIn("crates/widget-store/src/lib.rs:5", failures[0])

    def test_fleet_policy_tables_are_not_durable_write_stamps(self):
        self.write(
            "crates/lash-core-store/src/store/fleet_format.rs",
            """\
            pub const RECORD_UPCASTERS: &[RecordUpcaster] = &[RecordUpcaster {
                from_version: WIDGET_FORMAT_VERSION - 1,
            }];
            const WRITER_PINS: &[WriterPin] = &[WriterPin {
                version: WIDGET_FORMAT_VERSION,
            }];
            fn write() -> Row {
                Row { version: WIDGET_FORMAT_VERSION }
            }
            """,
        )
        failures = self.check()
        self.assertEqual(len(failures), 1)
        self.assertIn("fleet_format.rs:8", failures[0])

    def test_command_refuses_a_direct_stamp_on_a_source_declared_migrate_surface(self):
        self.declare()
        self.write("crates/widget-store/src/lib.rs", DIRECT_STAMP)
        status, errors = self.command()
        self.assertEqual(status, 1, errors)
        self.assertIn("crates/widget-store/src/lib.rs:6", errors)
        self.write("crates/widget-store/src/lib.rs", ROUTED_STAMP)
        status, errors = self.command()
        self.assertEqual(status, 0, errors)

    def test_command_refuses_to_pass_without_a_migrate_surface(self):
        self.declare("coexist")
        self.write("crates/widget-store/src/lib.rs", DIRECT_STAMP)
        status, errors = self.command()
        self.assertEqual(status, 1, errors)
        self.assertIn("no source-declared migrate surface", errors)

    def test_command_fails_when_source_discovery_fails(self):
        self.declare()
        self.write("crates/widget-store/src/lib.rs", ROUTED_STAMP)
        self.write(
            "crates/widget-store/src/other.rs", "pub const UNDECLARED_FORMAT_VERSION: u32 = 1;\n"
        )
        status, errors = self.command()
        self.assertEqual(status, 1, errors)
        self.assertIn("UNDECLARED_FORMAT_VERSION", errors)


if __name__ == "__main__":
    unittest.main()
