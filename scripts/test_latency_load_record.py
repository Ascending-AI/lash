#!/usr/bin/env python3
"""Self-test of scripts/latency_load_record.py against fake /proc files."""

import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

import latency_load_record as record

PRESSURE = ("some avg10=12.50 avg60=3.00 avg300=1.00 total=1000\n"
            "full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n")

# A stand-in for `lash-perf latency`: prints the case markers around a pause.
CHILD = """
import sys, time
for case in sys.argv[2:]:
    print(f"latency case {case}: started", flush=True)
    time.sleep(0.3)
    if sys.argv[1] == "dies" and case == "fast":
        sys.exit(7)
    print(f"latency case {case}: finished", flush=True)
print("latency gate: PASS", flush=True)
"""


class Host:
    """Fake /proc files in a temporary directory."""

    def __init__(self, directory, load1):
        self.loadavg = Path(directory) / "loadavg"
        self.pressure = Path(directory) / "cpu"
        self.loadavg.write_text(f"{load1:.2f} 2.00 1.00 3/900 4242\n")
        self.pressure.write_text(PRESSURE)


class LoadRecordTests(unittest.TestCase):
    def run_child(self, load1, *cases, mode="runs"):
        with tempfile.TemporaryDirectory() as directory:
            host = Host(directory, load1)
            out = io.StringIO()
            status, result = record.record(
                [sys.executable, "-c", CHILD, mode, *cases], interval=0.05,
                loadavg=host.loadavg, pressure=host.pressure, out=out)
            return status, result, out.getvalue()

    def test_subsecond_cases_sample_both_edges_without_inventing_interval_coverage(self):
        with tempfile.TemporaryDirectory() as directory:
            host = Host(directory, 1.0)
            _, result = record.record(
                [sys.executable, "-c", CHILD, "runs", "grace"], interval=60.0,
                loadavg=host.loadavg, pressure=host.pressure, out=io.StringIO())
        edges = [row for row in result["samples"] if row.get("case") == "grace"]
        self.assertEqual(["started", "finished"], [row["edge"] for row in edges])
        self.assertEqual(0, result["cases"]["grace"]["load"]["interval_samples"])
        self.assertEqual(2, result["cases"]["grace"]["load"]["samples"])

    def test_load_never_changes_the_exit_status(self):
        """NO-LOAD-GATE (Sam, 2026-10-19): the record is evidence beside the
        run; a loaded host neither fails a passing run nor masks a failing
        one, and every case window still carries its load."""

        status, result, output = self.run_child(500.0, "fast", "stream")
        self.assertEqual(0, status)
        self.assertEqual(0, result["exit_status"])
        self.assertNotIn("verdict", result)
        self.assertIn("latency gate: PASS", output, "the command's output is forwarded")
        for case in ("fast", "stream"):
            load = result["cases"][case]["load"]
            self.assertEqual(500.0, load["max_load1"])
            self.assertEqual(12.5, load["max_cpu_some_avg10"])
            self.assertGreaterEqual(load["samples"], 2)
        self.assertEqual(2.0, result["samples"][0]["load5"])

        status, result, _ = self.run_child(500.0, "fast", mode="dies")
        self.assertEqual(7, status, "a failed command keeps its own status")
        self.assertIsNone(result["cases"]["fast"]["finished_unix_s"])
        self.assertIsNone(result["cases"]["fast"]["load"]["max_load1"])

    def test_a_host_without_pressure_files_still_records_load(self):
        with tempfile.TemporaryDirectory() as directory:
            host = Host(directory, 1.0)
            row = record.sample(host.loadavg, Path(directory) / "absent", 1.0)
            self.assertIsNone(row["cpu_pressure"])
            self.assertEqual(1.0, row["load1"])

    def test_the_command_writes_the_record_and_prints_each_cases_load(self):
        with tempfile.TemporaryDirectory() as directory:
            host = Host(directory, 40.0)
            out = Path(directory) / "artifacts" / "latency-load.json"
            printed = io.StringIO()
            stdout, sys.stdout = sys.stdout, printed
            try:
                status = record.main([
                    "--out", str(out), "--interval", "0.05",
                    "--loadavg", str(host.loadavg), "--cpu-pressure", str(host.pressure),
                    "--", sys.executable, "-c", CHILD, "runs", "fast"])
            finally:
                sys.stdout = stdout
            self.assertEqual(0, status)
            written = json.loads(out.read_text())
            self.assertEqual(40.0, written["cases"]["fast"]["load"]["max_load1"])
            self.assertIn("latency load record: case fast: max load1 40.0", printed.getvalue())


if __name__ == "__main__":
    unittest.main()
