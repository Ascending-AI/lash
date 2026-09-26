#!/usr/bin/env python3
"""Fixture tests for check_writer_stamps.py."""

from __future__ import annotations

import importlib.util
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
        id: "restate.widget",
        name: "Restate widget",
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
        return gate.check(self.repo, [CONSTANT])

    def test_direct_write_stamp_is_flagged(self):
        self.write("crates/widget-store/src/lib.rs", DIRECT_STAMP)
        failures = self.check()
        self.assertEqual(len(failures), 1)
        self.assertIn("crates/widget-store/src/lib.rs:6", failures[0])
        self.assertIn("FleetFormat::writer_version", failures[0])

    def test_fleet_routed_stamp_is_not_flagged(self):
        self.write("crates/widget-store/src/lib.rs", ROUTED_STAMP)
        self.assertEqual(self.check(), [])

    def test_engine_format_manifest_rows_are_not_write_stamps(self):
        # The Restate engine's durable-format table declares the version the
        # build writes for the facade's manifest; the bytes themselves live in
        # the Restate deployment `F` does not govern. Same shape as the facade's
        # own `crates/lash/src/formats.rs` exemption.
        self.write("crates/lash-restate/src/formats.rs", MANIFEST_ROW)
        self.assertEqual(self.check(), [])

    def test_manifest_exemption_does_not_leak_to_other_paths(self):
        self.write("crates/widget-store/src/lib.rs", MANIFEST_ROW)
        failures = self.check()
        self.assertEqual(len(failures), 1)
        self.assertIn("crates/widget-store/src/lib.rs:5", failures[0])


if __name__ == "__main__":
    unittest.main()
