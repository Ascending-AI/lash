#!/usr/bin/env python3
"""Tests for the Buck2 event-log digest."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "buck2_event_digest", ROOT / "scripts/ci/buck2_event_digest.py"
)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class DigestTests(unittest.TestCase):
    @mock.patch.object(MODULE, "client", return_value="/tmp/buck2")
    @mock.patch.object(MODULE, "command")
    def test_digest_uses_the_event_log_for_summary_and_critical_path(
        self, command: mock.Mock, _client: mock.Mock
    ) -> None:
        command.side_effect = ["Actions: 12\nCache hits: 10", "header\none\ntwo"]
        lines = MODULE.digest(Path("events.pb.zst"))
        self.assertIn("  Actions: 12", lines)
        self.assertIn("  one", lines)
        self.assertEqual(
            command.call_args_list[0],
            mock.call("/tmp/buck2", "summary", "events.pb.zst"),
        )
        self.assertEqual(command.call_args_list[1].args[-1], "events.pb.zst")


if __name__ == "__main__":
    unittest.main()
