"""FIG-7478: workload changes preserve the instruction guard on other rows."""
import copy
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from perf_gate_baseline import calibrate_changed, references

spec = importlib.util.spec_from_file_location("perf_gates", Path(__file__).with_name("perf-gates.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class InstructionBaselineLaws(unittest.TestCase):
    def setUp(self):
        self.baseline = {"identity": {key: key for key in gate.IDENTITY}, "cpu": "pinned:0",
                         "geometry": {"kernel/python/same": {"source_hash": "same"},
                                      "kernel/python/changed": {"source_hash": "old"}},
                         "rows": {"kernel/python/same": 100, "kernel/python/changed": 100},
                         "provenance": {"commit": "old"}}
        self.candidate = copy.deepcopy(self.baseline)
        self.candidate["geometry"]["kernel/python/changed"]["source_hash"] = "new"
        self.candidate["rows"]["kernel/python/changed"] = 1000
        self.candidate["provenance"] = {"commit": "new"}

    def test_changed_workload_does_not_hide_unchanged_regression(self):
        self.candidate["rows"]["kernel/python/same"] = 102
        failures = gate.regressions(self.baseline, self.candidate, "instructions")
        self.assertEqual([row[0] for row in failures], ["kernel/python/same"])

    def test_changed_row_is_gated_against_its_committed_replacement(self):
        committed = calibrate_changed(self.baseline, self.candidate)
        self.candidate["rows"]["kernel/python/changed"] = 1011
        selected = references(self.baseline, self.candidate, committed)
        self.assertEqual([row[0] for row in gate.regressions(selected, self.candidate, "instructions")],
                         ["kernel/python/changed"])
        self.assertEqual(selected["row_verdicts"]["kernel/python/changed"], "rebaselined: geometry changed")
        self.assertEqual(selected["rows"]["kernel/python/same"], 100)

    def test_changed_added_and_removed_rows_require_committed_population(self):
        with self.assertRaises(ValueError):
            references(self.baseline, self.candidate, self.baseline)
        for added in (True, False):
            candidate = copy.deepcopy(self.baseline)
            if added:
                candidate["geometry"]["kernel/python/added"] = {"source_hash": "added"}
                candidate["rows"]["kernel/python/added"] = 10
            else:
                del candidate["geometry"]["kernel/python/changed"]
                del candidate["rows"]["kernel/python/changed"]
            with self.subTest(added=added):
                with self.assertRaises(ValueError):
                    references(self.baseline, candidate, self.baseline)
                committed = calibrate_changed(self.baseline, candidate)
                selected = references(self.baseline, candidate, committed)
                self.assertEqual(gate.regressions(selected, candidate, "instructions"), [])

    def test_calibration_preserves_unchanged_values_and_provenance(self):
        self.candidate["rows"]["kernel/python/same"] = 99
        refreshed = calibrate_changed(self.baseline, self.candidate)
        self.assertEqual(refreshed["rows"]["kernel/python/same"], 100)
        self.assertEqual(refreshed["rows"]["kernel/python/changed"], 1000)
        self.assertEqual(refreshed["row_provenance"].get("kernel/python/same", refreshed["provenance"]), {"commit": "old"})
        self.assertEqual(refreshed["row_provenance"]["kernel/python/changed"], {"commit": "new"})

    def test_identity_and_cpu_mismatches_still_refuse_the_whole_gate(self):
        for field in ("identity", "cpu"):
            candidate = copy.deepcopy(self.candidate)
            candidate[field] = {**candidate[field], "host": "different"} if field == "identity" else "different"
            with self.subTest(field=field), self.assertRaises(ValueError):
                gate.regressions(self.baseline, candidate, "instructions")
            with self.subTest(field=field, selection=True), self.assertRaises(ValueError):
                references(self.baseline, candidate, candidate)

    def test_calibration_cannot_replace_an_unchanged_regression(self):
        self.candidate["rows"]["kernel/python/same"] = 102
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            baseline = root / "baseline.json"
            original = json.dumps(self.baseline)
            baseline.write_text(original)
            out = root / "capture"
            args = ["perf-gates.py", "instructions", "--binary", "unused", "--worker", "unused",
                    "--out", str(out), "--baseline", str(baseline), "--calibrate"]
            with mock.patch("sys.argv", args), mock.patch.object(gate, "instructions", return_value=self.candidate), \
                    contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(gate.main(), 1)
            refreshed = json.loads(baseline.read_text())
            self.assertEqual(refreshed["rows"]["kernel/python/same"], 100)
            self.assertEqual(refreshed["rows"]["kernel/python/changed"], 1000)
            decision = json.loads((out / "decision.json").read_text())
            self.assertEqual([r[0] for r in decision["regressed_rows"]], ["kernel/python/same"])


if __name__ == "__main__":
    unittest.main()
