#!/usr/bin/env python3
"""Tests for scripts/ci/check_test_shard_coverage.py (FIG-3572)."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import re
import tempfile
import unittest
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "check_test_shard_coverage", ROOT / "scripts/ci/check_test_shard_coverage.py"
)
coverage = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(coverage)

LABEL = "//crates/example:suite__test"
CASES = [f"module::case_{index}" for index in range(12)]


def terse(names: list[str]) -> str:
    return "".join(f"{name}: test\n" for name in names) + "bench_like: bench\n"


class ShardCoverageTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp())
        self.testlogs = self.tmp / "testlogs"
        self.labels = self.tmp / "labels.txt"
        self.labels.write_text(f"{LABEL}\n//crates/example:plain__test\n")
        self.inventory = self.tmp / "inventory.json"
        self.inventory.write_text(
            json.dumps(
                {
                    "packages": [
                        {
                            "targets": [
                                {"label": LABEL, "shard_count": 3},
                                {"label": "//crates/example:plain__test"},
                            ]
                        }
                    ]
                }
            )
        )

    def write_shard(self, index: int, total: int, ran: list[str], *, zipped=False,
                    listed: list[str] | None = None) -> None:
        outputs = self.testlogs / "crates/example/suite__test" / (
            f"shard_{index + 1}_of_{total}"
        ) / "test.outputs"
        outputs.mkdir(parents=True, exist_ok=True)
        files = {
            "all.txt": terse(CASES if listed is None else listed),
            f"shard-{index}-of-{total}.txt": terse(ran),
        }
        if zipped:
            with zipfile.ZipFile(outputs / "outputs.zip", "w") as archive:
                for name, text in files.items():
                    archive.writestr(f"shard-coverage/{name}", text)
        else:
            (outputs / "shard-coverage").mkdir()
            for name, text in files.items():
                (outputs / "shard-coverage" / name).write_text(text)

    def run_check(self) -> tuple[int, str]:
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr), contextlib.redirect_stdout(io.StringIO()):
            code = coverage.main([
                "--labels", str(self.labels),
                "--testlogs", str(self.testlogs),
                "--inventory", str(self.inventory),
            ])
        return code, stderr.getvalue()

    def test_a_partition_passes_in_either_output_layout(self) -> None:
        self.write_shard(0, 3, CASES[:4])
        self.write_shard(1, 3, CASES[4:9], zipped=True)
        self.write_shard(2, 3, CASES[9:])
        self.assertEqual((0, ""), self.run_check())

    def test_a_case_no_shard_ran_fails(self) -> None:
        self.write_shard(0, 3, CASES[:4])
        self.write_shard(1, 3, CASES[4:9])
        self.write_shard(2, 3, CASES[9:11])
        code, errors = self.run_check()
        self.assertEqual(1, code)
        self.assertIn("1 listed case(s) ran in no shard: module::case_11", errors)

    def test_a_case_two_shards_ran_fails(self) -> None:
        self.write_shard(0, 3, CASES[:5])
        self.write_shard(1, 3, CASES[4:9])
        self.write_shard(2, 3, CASES[9:])
        code, errors = self.run_check()
        self.assertEqual(1, code)
        self.assertIn("`module::case_4` ran in shards 1 and 2", errors)

    def test_a_missing_shard_fails(self) -> None:
        self.write_shard(0, 3, CASES[:4])
        self.write_shard(2, 3, CASES[9:])
        code, errors = self.run_check()
        self.assertEqual(1, code)
        self.assertIn("shard 2/3 left no `--list` record", errors)

    def test_shards_that_disagree_on_the_list_fail(self) -> None:
        self.write_shard(0, 3, CASES[:4])
        self.write_shard(1, 3, CASES[4:9], listed=CASES[:-1])
        self.write_shard(2, 3, CASES[9:])
        code, errors = self.run_check()
        self.assertEqual(1, code)
        self.assertIn("shard 2/3 saw a different `--list`", errors)

    def test_the_postgres_long_poles_are_sharded_and_checked(self) -> None:
        counts = coverage.shard_counts(json.loads(coverage.INVENTORY.read_text()))
        labels = (ROOT / "tools/bazel/postgres_test_labels.txt").read_text().split()
        for label in (
            "//crates/lash-postgres-store:conformance__test",
            "//crates/lash-postgres-store:integration__test",
        ):
            with self.subTest(label=label):
                self.assertIn(label, labels)
                self.assertGreater(counts.get(label, 0), 1)

    def test_store_tests_runs_the_check_after_the_slotted_run(self) -> None:
        script = (ROOT / "scripts/ci/store-tests.sh").read_text()
        arm = script.split("\n  pg-store)\n", 1)[1].split("\n    ;;", 1)[0]
        trusted = arm.split("else", 1)[0]
        self.assertIn("--run_under=//tools/bazel:postgres_slot_runner", trusted)
        self.assertIn('--local_test_jobs="${slots}"', trusted)
        self.assertLess(
            trusted.index("bazel_test"),
            trusted.index("check_test_shard_coverage.py"),
        )
        # The slot count is the wrapper's, and the runner names the slots the
        # wrapper creates.
        wrapper = (ROOT / "scripts/ci/with-service.sh").read_text()
        self.assertRegex(wrapper, r"readonly POSTGRES_SLOT_COUNT=\d+")
        self.assertIn('CREATE DATABASE lash_slot_${index}', wrapper)
        runner = (ROOT / "tools/bazel/postgres_slot_runner.sh").read_text()
        self.assertIn("lash_slot_${slot}", runner)
        self.assertTrue(re.search(r'exec "\$\{here\}/test_xml_runner\.sh" "\$@"', runner))


if __name__ == "__main__":
    unittest.main()
