#!/usr/bin/env python3
"""Contract tests for the small Bazel profile digest."""

import importlib.util
import gzip
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).parent / "ci" / "bazel_profile_digest.py"
SPEC = importlib.util.spec_from_file_location("bazel_profile_digest", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class BazelProfileDigestTests(unittest.TestCase):
    def test_critical_path_and_overlapping_queue_events(self) -> None:
        profile = {"traceEvents": [
            {"cat": "critical path component", "name": "first", "dur": 2_000_000},
            {"cat": "critical path component", "name": "second", "dur": 3_000_000},
            {"cat": "Remote execution queuing time", "dur": 1_000_000},
            {"cat": "Remote execution queuing time", "dur": 4_000_000},
            {"cat": "remote action execution", "dur": 20_000_000},
        ]}
        lines = MODULE.digest(profile)
        self.assertIn("5.00s across 2 components", lines[0])
        self.assertIn("3.00s  second", lines[1])
        self.assertIn("p95 4.00s; max 4.00s", lines[-1])
        self.assertIn("events may overlap", lines[-1])

    def test_missing_queue_events_and_invalid_trace(self) -> None:
        self.assertIn("no timing events", MODULE.digest({"traceEvents": []})[-1])
        with self.assertRaises(ValueError):
            MODULE.digest({"traceEvents": {}})

    def test_gzipped_profile_writes_step_summary(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            profile = Path(directory) / "profile.json.gz"
            summary = Path(directory) / "summary.md"
            with gzip.open(profile, "wt", encoding="utf-8") as stream:
                json.dump({"traceEvents": [
                    {"cat": "critical path component", "name": "compile", "dur": 750_000},
                ]}, stream)
            result = subprocess.run(
                [sys.executable, str(SCRIPT), str(profile), "--summary", str(summary)],
                text=True, capture_output=True, check=True,
            )
            self.assertIn("0.75s across 1 component", result.stdout)
            self.assertIn("0.75s  compile", summary.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
