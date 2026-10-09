#!/usr/bin/env python3
"""PERF-OUTPUTS: profiling uses report artifacts, regardless of Cargo paths."""
import argparse
import json
import tempfile
import unittest
from pathlib import Path

from perf_artifacts import artifacts
from profile_runtime import resolve_binary as runtime_binary
from profile_runtime_stack import resolve_binary as stack_binary


class MaterializedProfilingOutputs(unittest.TestCase):
    def test_recipes_resolve_materialized_outputs_without_cargo_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            labels = ["//crates/lash-perf:lash-perf__bin"] + [
                f"//crates/lashlang:{name}__example"
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
                                      release=True, cpu_profile=False)
            self.assertEqual(runtime_binary(args, root), expected[labels[0]])
            self.assertEqual(stack_binary(args, root), expected[labels[0]])
            self.assertEqual(artifacts(root, labels[1:], build=False, report=report,
                                      optimized=True), {label: expected[label] for label in labels[1:]})
            self.assertFalse((root / "target").exists())


if __name__ == "__main__":
    unittest.main()
