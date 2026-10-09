#!/usr/bin/env python3
"""PERF-OUTPUTS: profiling uses report artifacts, regardless of Cargo paths."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

import profile_runtime
from perf_capture import blocked_stacks, run_profiled
from profile_diff import differential, sample_commits

from perf_artifacts import artifacts
from profile_runtime import resolve_binary as runtime_binary
from profile_runtime_stack import resolve_binary as stack_binary


class MaterializedProfilingOutputs(unittest.TestCase):
    def test_recipes_resolve_materialized_outputs_without_cargo_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            labels = ["//crates/lash-perf:lash-perf__bin"] + [
                f"//crates/lash-vm:{name}__example"
                for name in ("perf", "profile", "function_perf")
            ]
            results = {}
            expected = {}
            for index, label in enumerate(labels):
                executable = root / f"buck-output-{index}"
                executable.touch()
                expected[label] = executable
                results["root" + label] = {
                    "success": "SUCCESS", "outputs": {"DEFAULT": [executable.name]}
                }
            report = root / "report.json"
            report.write_text(json.dumps({"project_root": str(root), "results": results}))
            args = argparse.Namespace(binary=None, cargo_feature=[], dhat=False,
                                      build=False, build_report=report,
                                      release=True, cpu_profile=False, off_cpu=False)
            self.assertEqual(runtime_binary(args, root), expected[labels[0]])
            self.assertEqual(stack_binary(args, root), expected[labels[0]])
            self.assertEqual(artifacts(root, labels[1:], build=False, report=report,
                                      optimized=True), {label: expected[label] for label in labels[1:]})
            self.assertFalse((root / "target").exists())


class SamplingLaws(unittest.TestCase):
    def test_cpu_profile_records_workload_instead_of_only_selecting_symbols(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            receipt = root / "runtime.json"
            with mock.patch("sys.argv", ["profile_runtime.py", "--cpu-profile",
                                         "--no-build", "--out", str(receipt),
                                         "--scenario", "standard", "--profile", "quick"]):
                args = profile_runtime.parse_args()
            calls = []

            def execute(cmd, **kwargs):
                calls.append(cmd)
                if "record" in cmd:
                    Path(cmd[cmd.index("-o") + 1]).write_bytes(b"sample")
                if "script" in cmd:
                    stdout = "lash-perf 1 1.000: cycles:u:\n        123 lash_perf::run (lash-perf)\n\n"
                elif "inferno-collapse-perf" in str(cmd[0]):
                    stdout = "lash-perf;lash_perf::run 1\n"
                elif "c++filt" in str(cmd[0]):
                    stdout = kwargs["input"]
                elif "report" in cmd:
                    stdout = " 100.00% lash-perf lash-perf [.] lash_perf::run\n"
                else:
                    stdout = json.dumps({"out": str(receipt)})
                if kwargs.get("stdout") is not None:
                    kwargs["stdout"].write(stdout)
                    stdout = None
                return subprocess.CompletedProcess(cmd, 0, stdout, "")

            with mock.patch.object(profile_runtime, "parse_args", return_value=args), \
                 mock.patch.object(profile_runtime, "resolve_binary", return_value=Path(__file__)), \
                 mock.patch("subprocess.run", side_effect=execute):
                self.assertEqual(profile_runtime.main(), 0)
            records = [cmd for cmd in calls if "record" in cmd]
            self.assertEqual(len(records), 1, "--cpu-profile must start perf record")
            self.assertIn("-g", records[0])
            self.assertIn("cycles/name=cycles,freq=999/u", records[0])
            self.assertEqual(records[0][records[0].index("-c") + 1], "1")
            self.assertEqual(records[0][records[0].index("--call-graph") + 1], "fp")
            self.assertIn("standard", records[0])
            self.assertTrue((receipt.with_suffix(".profiles") / "cpu.folded").exists())
            self.assertTrue((receipt.with_suffix(".profiles") / "cpu.top.txt").exists())

    def test_unavailable_perf_cannot_leave_a_successful_capture_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            receipt = Path(directory) / "runtime.json"
            with mock.patch("shutil.which", return_value=None), self.assertRaisesRegex(SystemExit, "missing"):
                run_profiled(["workload"], receipt=receipt, cpu=True)
            capture = json.loads((receipt.with_suffix(".profiles") / "capture.json").read_text())
            self.assertEqual(capture["status"], "failed")
            self.assertIn("missing", capture["reason"])

    def test_blocked_time_uses_switch_in_per_tid_and_excludes_preemption(self):
        text = """worker 10/11 1.000000000: sched:sched_switch: prev_pid=11 prev_state=S ==> next_pid=99
        abc lash_perf::wait (lash-perf)
        def lash_perf::run (lash-perf)

worker 10/12 1.100000000: sched:sched_switch: prev_pid=12 prev_state=R ==> next_pid=99
        abc lash_perf::spin (lash-perf)

worker 10/12 1.200000000: PERF_RECORD_SWITCH IN
worker 10/11 1.500000000: PERF_RECORD_SWITCH IN
worker 10/11 2.000000000: sched:sched_switch: prev_pid=11 prev_state=D ==> next_pid=99
        abc lash_perf::unfinished (lash-perf)
"""
        stacks, incomplete = blocked_stacks(text)
        self.assertEqual(stacks, {"worker;lash_perf::run;lash_perf::wait": 500_000_000})
        self.assertEqual(incomplete, 1)

    def test_differential_ranks_absolute_changes_from_inferno_columns(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with mock.patch("shutil.which", return_value="inferno-diff-folded"), \
                 mock.patch("subprocess.run", return_value=subprocess.CompletedProcess(
                     [], 0, "run;grow 1 9\nrun;shrink 10 0\nrun;same 5 5\n", "")):
                report = differential(root / "a", root / "b", root, normalize=False, svg=False, top=2)
            self.assertEqual(report.splitlines()[1:], ["-10 10 0 run;shrink", "+8 1 9 run;grow"])


    def test_commit_comparison_restores_the_checkout_after_a_build_failure(self):
        calls = []
        revision = "a" * 40

        def git(cmd, **kwargs):
            calls.append(cmd)
            return subprocess.CompletedProcess(cmd, 0, revision if "rev-parse" in cmd else "", "")

        with tempfile.TemporaryDirectory() as directory, \
             mock.patch("profile_diff.checked", side_effect=git), \
             mock.patch("profile_diff.subprocess.run", return_value=subprocess.CompletedProcess(
                 [], 0, "review-fork\n", "")), \
             mock.patch("profile_diff.artifacts", side_effect=RuntimeError("build failed")), \
             self.assertRaisesRegex(RuntimeError, "build failed"):
            sample_commits(Path(directory), ["before", "after"], Path(directory), "standard", 1, 1)
        self.assertIn(["git", "switch", "--detach", revision], calls)
        self.assertEqual(calls[-1], ["git", "switch", "review-fork"])


if __name__ == "__main__":
    unittest.main()
