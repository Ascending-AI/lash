"""FIG-7478: workload changes preserve the instruction guard on other rows."""
import copy
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import subprocess
import sys
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

    def test_overrun_reports_failure_and_runs_remaining_rows(self):
        """FIG-7434: one capped producer must not erase or abort later rows."""
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            policy = root / "crates/lash-perf/workloads/replay"
            policy.mkdir(parents=True)
            (policy / "budgets.json").write_text(json.dumps({"subset": [{"workload": "chat"}]}))
            (policy / "chat.json").write_text(json.dumps({"protocol": "standard"}))
            names = []

            def producer(command, **kwargs):
                measured = command[command.index("--") + 1:] if command[0] == "perf" else command
                if measured[0] == "timeout":
                    measured = measured[5:]
                if "--workload" in measured:
                    dialect = measured[measured.index("--dialect") + 1] if "--dialect" in measured else "typescript"
                    names.append(dialect)
                    if dialect == "typescript":
                        if command[0] != "perf":
                            raise subprocess.TimeoutExpired(command, 1200)
                        Path(command[command.index("-o") + 1]).write_text("987654;;instructions:u;1000000000;100.00;;\n")
                        return subprocess.CompletedProcess(command, 124)
                    capture = {"cpu": "pinned:0", "commit": "new", "recorded_at": "now", "run_id": "test",
                               "geometry": {"warmups": 1}, "errors": [], "rows": [{
                                   "engine": "lash", "dialect": dialect, "workload": "same", "source_hash": "same",
                                   "trips": [1, 100], "operations": [1, 100], "engine_identity": "binary",
                                   "samples": [[{"instruction_window": {"time_enabled": 1, "time_running": 1, "instructions": 10}}] * 2] * 5,
                                   "instructions": {"median_per_operation": 100}}]}
                else:
                    names.append("replay")
                    capture = {"identity": {**{key: key for key in gate.IDENTITY}, "executable_sha256": "binary"},
                               "workloads": [{"experiment": {}, "summary": {}, "protocol": "standard", "script_digest": "script",
                                              "backend": "sqlite", "turns": 1, "sessions": 1,
                                              "rounds": [{"sessions": 1, "metrics": {"host_instruction_enabled_ns": 1,
                                                        "host_instruction_running_ns": 1, "host_instructions": 100}}]}]}
                Path(measured[measured.index("--out") + 1]).write_text(json.dumps(capture))
                return subprocess.CompletedProcess(command, 0)

            geometry = {f"kernel/{dialect}/same": {"source_hash": "same", "trips": [1, 100],
                        "operations": [1, 100], "runs": 5, "warmups": 1} for dialect in ("typescript", "python")}
            with mock.patch.object(gate, "ROOT", root), mock.patch.object(gate, "kernel_workloads", return_value={"same": (1, 1, {})}), \
                    mock.patch.object(gate, "kernel_geometry", return_value=geometry, create=True), \
                    mock.patch.object(gate.subprocess, "run", side_effect=producer):
                sample = gate.instructions("unused", root, {})
            self.assertEqual(names, ["typescript", "python", "replay"])
            self.assertEqual(sample["rows"], {"kernel/python/same": 100, "replay/chat/host": 100})
            capped = sample["row_failures"]["kernel/typescript/same"]
            self.assertEqual(capped["reason"], "exceeded cap")
            self.assertEqual(capped["instructions"], 987654)
            self.assertNotIn("kernel/typescript/same", sample["rows"])

    def test_capped_sample_fails_both_gates_without_advancing_baseline_or_history(self):
        """FIG-7434: a completed population with one overrun is a failure, never a baseline."""
        candidate = copy.deepcopy(self.baseline)
        del candidate["rows"]["kernel/python/changed"]
        candidate["row_failures"] = {"kernel/python/changed": {
            "reason": "exceeded cap", "cap_seconds": 1200, "instructions": 987654}}
        candidate["provenance"] = {"commit": "new"}
        candidate["binaries"] = gate.binary_identity(sys.executable, sys.executable)
        for mode in ("instructions", "history"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                committed = root / "docs/perf/baselines/instructions.json"
                committed.parent.mkdir(parents=True)
                committed.write_text(json.dumps(self.baseline))
                baseline = root / "baseline.json"
                original = json.dumps(self.baseline)
                baseline.write_text(original)
                sample = root / "sample.json"
                sample.write_text(json.dumps(candidate))
                out, history = root / "capture", root / "history.jsonl"
                args = ["perf-gates.py", mode, "--binary", sys.executable, "--worker", sys.executable,
                        "--out", str(out), "--baseline", str(baseline), "--history", str(history)]
                if mode == "history":
                    args += ["--sample", str(sample)]

                def git_output(command, **kwargs):
                    return "" if "status" in command else "new\n"

                output = io.StringIO()
                with mock.patch("sys.argv", args), mock.patch.object(gate, "ROOT", root), \
                        mock.patch.object(gate, "instructions", return_value=candidate) as producer, \
                        mock.patch.object(gate.subprocess, "check_output", side_effect=git_output), \
                        mock.patch.object(gate.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)), \
                        mock.patch.object(gate, "execute") as record, contextlib.redirect_stdout(output):
                    self.assertEqual(gate.main(), 1)
                self.assertEqual(baseline.read_text(), original)
                self.assertFalse(history.exists())
                record.assert_not_called()
                if mode == "history":
                    producer.assert_not_called()
                decision = json.loads((out / "decision.json").read_text())
                self.assertEqual(decision["outcome"], "fail")
                self.assertEqual(decision["row_failures"], candidate["row_failures"])
                self.assertIn("kernel/python/same | PASS", output.getvalue())
                self.assertIn("kernel/python/changed | FAIL; exceeded cap", output.getvalue())

    def test_history_rejects_sample_from_another_commit_or_executable(self):
        """Reusing a population must not assign old measurements to current main."""
        for mismatch in ("commit", "binaries"):
            with self.subTest(mismatch=mismatch), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                sample = root / "sample.json"
                candidate = copy.deepcopy(self.baseline)
                candidate["provenance"] = {"commit": "new"}
                candidate["binaries"] = gate.binary_identity(sys.executable, sys.executable)
                if mismatch == "commit":
                    candidate["provenance"]["commit"] = "old"
                else:
                    candidate["binaries"]["worker"] = "different"
                sample.write_text(json.dumps(candidate))
                out = root / "capture"
                args = ["perf-gates.py", "history", "--binary", sys.executable, "--worker", sys.executable,
                        "--out", str(out), "--sample", str(sample)]
                with mock.patch("sys.argv", args), mock.patch.object(gate, "execute") as record, \
                        mock.patch.object(gate.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)), \
                        mock.patch.object(gate.subprocess, "check_output", side_effect=lambda cmd, **kw: "" if "status" in cmd else "new\n"), \
                        contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(gate.main(), 2)
                record.assert_not_called()
                self.assertEqual(json.loads((out / "decision.json").read_text())["outcome"], "refused")

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
                    mock.patch.object(gate, "binary_identity", return_value={}), \
                    contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(gate.main(), 1)
            refreshed = json.loads(baseline.read_text())
            self.assertEqual(refreshed["rows"]["kernel/python/same"], 100)
            self.assertEqual(refreshed["rows"]["kernel/python/changed"], 1000)
            decision = json.loads((out / "decision.json").read_text())
            self.assertEqual([r[0] for r in decision["regressed_rows"]], ["kernel/python/same"])


if __name__ == "__main__":
    unittest.main()
