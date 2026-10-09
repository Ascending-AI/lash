#!/usr/bin/env python3
"""VM receipts certify complete work and label configured stack capacity."""
import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import profile_lash_vm as vm
import perfreport


class VmCertificationTests(unittest.TestCase):
    def profile_verdict(self, total):
        row = vm.parse_profile_output(
            f"iterations: 2\nvm_instructions_total: {total}\n"
            "instruction_hotspots:\nShown count=2 total_ms=1 avg_ns=1\n")
        row["scenario_arg"] = "baseline"
        budgets = {"lashvm": {"profile": {"baseline": {"instructions_per_iter_max": 10}}}}
        return next(r for r in vm.evaluate_lash_vm_budgets(
            {"profile_results": [row]}, budgets) if r["metric"] == "instructions_per_iter")

    def test_opcode_guard_uses_full_total_beyond_displayed_hotspots(self):
        verdict = self.profile_verdict(200)
        self.assertEqual(verdict["actual"], 100)
        self.assertFalse(verdict["passed"])

    def test_missing_full_opcode_total_cannot_certify(self):
        verdict = self.profile_verdict("missing")
        self.assertFalse(verdict["passed"])
        self.assertIn("vm_instructions_total", verdict["reason"])

    def test_only_selected_populations_certify_and_missing_selected_ratios_fail(self):
        budgets = {"lashvm": {
            "profile": {"baseline": {"instructions_per_iter_max": 10}},
            "perf": {"default": {"allocated_bytes_per_iter_max": 10, "allocations_per_iter_max": 10}},
            "perf_ratios": {"scaling": {"mode": "compiled_execute", "numerator": "large", "denominator": "small", "max": 2}},
        }}
        profile = {"scenario_arg": "baseline", "iterations": 2, "vm_instructions_total": 20}
        verdicts = vm.evaluate_lash_vm_budgets({
            "parameters": {"skip_perf": True}, "profile_results": [profile]}, budgets)
        self.assertEqual(len(verdicts), 1)
        self.assertTrue(verdicts[0]["passed"])
        row = {"scenario_arg": "other", "mode_arg": "one_shot",
               "allocated_bytes_per_iter": 1, "allocations_per_iter": 1}
        verdicts = vm.evaluate_lash_vm_budgets({
            "parameters": {"skip_profile": True, "scenarios": ["other"], "modes": ["one_shot"]},
            "perf_results": [row]}, budgets)
        self.assertTrue(all(verdict["passed"] for verdict in verdicts))
        verdicts = vm.evaluate_lash_vm_budgets({
            "parameters": {"skip_profile": True, "scenarios": ["large", "small"], "modes": ["compiled_execute"]},
            "perf_results": [row]}, budgets)
        ratio = next(verdict for verdict in verdicts if verdict["section"] == "perf_ratio")
        self.assertFalse(ratio["passed"])
        self.assertEqual(ratio["reason"], "missing ratio measurement")

    def invoke(self, mode):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory) / "receipt.json"
            args = ["profile_lash_vm.py", *([mode] if mode else []), "--skip-perf", "--profile-scenario", "baseline",
                    "--profile-iterations", "1", "--out", str(out)]
            executable = Path(__file__)
            bins = {f"//crates/lash-vm:{n}__example": executable
                    for n in ("perf", "profile", "function_perf")}
            stdout = io.StringIO()
            with patch("sys.argv", args), patch.object(vm, "artifacts", return_value=bins), \
                    patch.object(vm, "apply_stack_budget"), \
                    patch.object(vm, "load_scenarios", return_value=["baseline"]), \
                    patch.object(vm, "run_command", return_value="iterations: 1\nvm_instructions_total: 10000000\n"), \
                    contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(io.StringIO()):
                status = vm.main()
            return status, json.loads(out.read_text()), stdout.getvalue()

    def test_certifying_invocation_fails_any_selected_budget(self):
        for mode in ("--enforce-budgets", None):
            status, receipt, _ = self.invoke(mode)
            self.assertEqual(status, 1)
            self.assertEqual(receipt["certification_mode"], "certifying")

    def test_report_only_invocation_explicitly_does_not_certify(self):
        status, receipt, output = self.invoke("--report-only")
        self.assertEqual(status, 0)
        self.assertEqual(receipt["certification_mode"], "report_only")
        self.assertIn("does not certify", output)


class ConfiguredStackCapacityTest(unittest.TestCase):
    def test_configured_stack_does_not_certify_usage(self):
        with patch.dict("os.environ", {"RUST_MIN_STACK": "4096"}), patch.object(
            vm, "process_stack_limits", return_value=(8192, None, True)
        ):
            profile = vm.current_stack_profile(2048)
        print(json.dumps(profile, sort_keys=True))
        self.assertEqual(profile.get("configured_stack_capacity_bytes"), 4096)
        self.assertEqual(profile.get("configured_stack_capacity_source"), "rust_min_stack")
        self.assertNotIn("within_stack_budget", profile)
        self.assertNotIn("measured_stack_bytes", profile)
        summary = perfreport.fmt_stack_profile(profile)
        self.assertIn("configured_capacity=4.00KiB", summary)
        self.assertIn("configured_budget=2.00KiB", summary)
        self.assertNotIn("within_budget", summary)

    def test_process_limit_is_configured_capacity_when_thread_default_is_unknown(self):
        with patch.dict("os.environ", {"RUST_MIN_STACK": "invalid"}), patch.object(
            vm, "process_stack_limits", return_value=(8192, None, True)
        ):
            profile = vm.current_stack_profile(2048)
        self.assertEqual(profile.get("configured_stack_capacity_bytes"), 8192)
        self.assertEqual(profile.get("configured_stack_capacity_source"), "process_stack_soft_limit")

    def test_unknown_capacity_stays_unknown(self):
        with patch.dict("os.environ", {"RUST_MIN_STACK": "invalid"}), patch.object(
            vm, "process_stack_limits", return_value=(None, None, True)
        ):
            profile = vm.current_stack_profile(2048)
        self.assertIsNone(profile["configured_stack_capacity_bytes"])
        self.assertIsNone(profile["configured_stack_capacity_source"])


if __name__ == "__main__":
    unittest.main()
