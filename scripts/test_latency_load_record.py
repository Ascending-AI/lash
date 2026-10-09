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
    def run_child(self, load1, *cases, mode="runs", cores=32):
        with tempfile.TemporaryDirectory() as directory:
            host = Host(directory, load1)
            out = io.StringIO()
            status, result = record.record(
                [sys.executable, "-c", CHILD, mode, *cases], interval=0.05, case="fast",
                cores=cores, loadavg=host.loadavg, pressure=host.pressure, out=out)
            return status, result, out.getvalue()

    def test_a_quiet_fast_case_qualifies(self):
        status, result, output = self.run_child(3.0, "fast")
        self.assertEqual(0, status)
        verdict = result["verdict"]
        self.assertTrue(verdict["qualified"], verdict)
        self.assertEqual(3.0, verdict["max_load1"])
        self.assertEqual(12.5, verdict["max_cpu_some_avg10"])
        self.assertGreaterEqual(verdict["samples_in_window"], 2)
        self.assertIn("latency gate: PASS", output, "the command's output is forwarded")
        self.assertEqual(12.5, result["samples"][0]["cpu_pressure"]["some_avg10"])
        self.assertEqual(2.0, result["samples"][0]["load5"])

    def test_load_at_the_core_count_during_the_fast_case_is_unqualified(self):
        status, result, _ = self.run_child(32.0, "fast")
        self.assertEqual(3, status, "an unqualified run cannot certify")
        self.assertFalse(result["verdict"]["qualified"])
        self.assertIn("reached 32.00", result["verdict"]["reason"])

    def test_load_just_below_the_core_count_qualifies(self):
        _, result, _ = self.run_child(31.99, "fast")
        self.assertTrue(result["verdict"]["qualified"])

    def test_only_the_fast_cases_window_is_judged(self):
        samples = [{"unix_s": 10.0, "load1": 90.0, "cpu_pressure": None},
                   {"unix_s": 20.0, "load1": 4.0, "cpu_pressure": None},
                   {"unix_s": 30.0, "load1": 90.0, "cpu_pressure": None}]
        windows = record.case_windows([
            {"case": "stream", "edge": "started", "unix_s": 5.0},
            {"case": "stream", "edge": "finished", "unix_s": 15.0},
            {"case": "fast", "edge": "started", "unix_s": 15.0},
            {"case": "fast", "edge": "finished", "unix_s": 25.0},
        ])
        verdict = record.qualify(samples, windows, "fast", 32)
        self.assertTrue(verdict["qualified"], verdict)
        self.assertEqual(1, verdict["samples_in_window"])
        self.assertFalse(record.qualify(samples, windows, "stream", 32)["qualified"])

    def test_a_run_without_the_fast_case_is_unqualified(self):
        _, result, _ = self.run_child(1.0, "stream")
        self.assertFalse(result["verdict"]["qualified"])
        self.assertIn("did not run", result["verdict"]["reason"])
        self.assertIn("stream", result["cases"])

    def test_a_fast_case_that_dies_is_unqualified_and_keeps_its_status(self):
        status, result, _ = self.run_child(1.0, "fast", mode="dies")
        self.assertEqual(7, status)
        self.assertFalse(result["verdict"]["qualified"])
        self.assertIn("did not finish", result["verdict"]["reason"])

    def test_a_host_without_pressure_files_still_records_load(self):
        with tempfile.TemporaryDirectory() as directory:
            host = Host(directory, 1.0)
            row = record.sample(host.loadavg, Path(directory) / "absent", 1.0)
            self.assertIsNone(row["cpu_pressure"])
            self.assertEqual(1.0, row["load1"])

    def test_the_command_writes_the_record_and_prints_its_verdict(self):
        with tempfile.TemporaryDirectory() as directory:
            host = Host(directory, 40.0)
            out = Path(directory) / "artifacts" / "latency-load.json"
            printed = io.StringIO()
            stdout, sys.stdout = sys.stdout, printed
            try:
                status = record.main([
                    "--out", str(out), "--interval", "0.05", "--cores", "32",
                    "--loadavg", str(host.loadavg), "--cpu-pressure", str(host.pressure),
                    "--", sys.executable, "-c", CHILD, "runs", "fast"])
            finally:
                sys.stdout = stdout
            self.assertEqual(3, status)
            written = json.loads(out.read_text())
            self.assertFalse(written["verdict"]["qualified"])
            self.assertEqual(32, written["verdict"]["cores"])
            self.assertIn("latency load record: UNQUALIFIED", printed.getvalue())


if __name__ == "__main__":
    unittest.main()
