#!/usr/bin/env python3
"""Tests for interruption-safe Buck2 diagnostic capture."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "buck2_diagnose", ROOT / "scripts/buck2-diagnose.py"
)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class DiagnosticTests(unittest.TestCase):
    def test_capture_owns_all_diagnostic_paths(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            args = SimpleNamespace(
                output=Path(temporary) / "bundle",
                baseline=None,
                operation="build",
                buck2_args=["--event-log", "/tmp/other.json-lines"],
            )
            with self.assertRaisesRegex(ValueError, "Capture owns --event-log"):
                MODULE.capture(args)

    def test_missing_event_log_keeps_build_status_and_marks_diagnostics_incomplete(self) -> None:
        child = SimpleNamespace(pid=123, wait=lambda: 7)
        with tempfile.TemporaryDirectory() as temporary, patch.object(
            MODULE, "source_identity", return_value={"revision": "same", "content_sha256": "same"}
        ), patch.object(MODULE.subprocess, "Popen", return_value=child):
            args = SimpleNamespace(
                output=Path(temporary) / "bundle",
                baseline=None,
                operation="build",
                buck2_args=["//:workspace_compile"],
            )
            self.assertEqual(MODULE.capture(args), 7)
            manifest = json.loads((args.output / "manifest.json").read_text())
            self.assertEqual(manifest["build_exit_code"], 7)
            self.assertEqual(manifest["state"], "failed")
            self.assertFalse(manifest["diagnostics_complete"])
            self.assertEqual(manifest["diagnostics_error"], "event log missing")
            command = manifest["command"]
            self.assertIn("--build-report", command)
            self.assertIn("--command-report-path", command)
            self.assertIn("--event-log", command)

    def test_child_signal_status_is_a_nonzero_shell_exit(self) -> None:
        child = SimpleNamespace(pid=123, wait=lambda: -9)
        with tempfile.TemporaryDirectory() as temporary, patch.object(
            MODULE, "source_identity", return_value={"revision": "same"}
        ), patch.object(MODULE.subprocess, "Popen", return_value=child):
            args = SimpleNamespace(
                output=Path(temporary) / "bundle",
                baseline=None,
                operation="build",
                buck2_args=["//:workspace_compile"],
            )
            self.assertEqual(MODULE.capture(args), 137)
            manifest = json.loads((args.output / "manifest.json").read_text())
            self.assertEqual(manifest["build_exit_code"], -9)
            self.assertEqual(manifest["state"], "failed")

    def test_compare_uses_two_explicit_event_logs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary, patch.object(
            MODULE, "buck2_client", return_value="/tmp/buck2"
        ), patch.object(MODULE.subprocess, "run") as run:
            root = Path(temporary)
            first = root / "first"
            second = root / "second"
            first.mkdir()
            second.mkdir()
            MODULE.compare(first, second)
            command = run.call_args.args[0]
            self.assertEqual(command[:4], ["/tmp/buck2", "log", "diff", "action-divergence"])
            self.assertIn(str((first / "events.json-lines").resolve()), command)
            self.assertIn(str((second / "events.json-lines").resolve()), command)


if __name__ == "__main__":
    unittest.main()
