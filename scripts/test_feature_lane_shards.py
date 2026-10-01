#!/usr/bin/env python3
"""The `feature-lanes` matrix proves exactly what the single job proved.

Sharding is a scheduling change, so the contract is that nothing falls between
the shards: every target of every lane aggregate, and every test-case floor,
belongs to exactly one shard of the real generated lists, and the workflow
runs exactly those shards.
"""

from __future__ import annotations

import collections
import json
from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock

import yaml

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts" / "ci"))

import check_feature_lane_test_floors as floors_check  # noqa: E402
import feature_lane_shards as shards  # noqa: E402

SCRIPT = ROOT / "scripts" / "ci" / "feature_lane_shards.py"
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
ALL_SHARDS = range(1, shards.SHARDS + 1)


def synthetic(lanes: dict[str, list[str]], **groups: list[str]) -> dict:
    members = sorted({label for labels in lanes.values() for label in labels})
    return {
        "packages": [],
        "feature_lanes": lanes,
        "feature_lane_compile_targets": groups.get("compile", members),
        "feature_lane_clippy_targets": groups.get("clippy", []),
        "feature_lane_test_targets": groups.get("tests", []),
        "feature_lane_test_floors": {},
    }


class RealPartitionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.inventory, cls.sizes = shards.load()
        cls.owner = shards.partition(cls.inventory, cls.sizes)

    def test_every_aggregate_is_covered_exactly_once(self) -> None:
        for group, key in shards.GROUPS.items():
            with self.subTest(group=group):
                members = self.inventory[key]
                self.assertTrue(members, f"{group} is empty")
                self.assertEqual(len(members), len(set(members)))
                selected = collections.Counter(
                    label
                    for shard in ALL_SHARDS
                    for label in shards.selection(self.inventory, self.sizes, group, shard)
                )
                self.assertEqual(collections.Counter(members), selected)

    def test_every_owner_is_a_shard_the_workflow_runs(self) -> None:
        self.assertEqual(set(ALL_SHARDS), set(self.owner.values()))
        self.assertEqual(
            {label for labels in self.inventory["feature_lanes"].values() for label in labels},
            set(self.owner),
        )

    def test_every_floor_is_held_once_by_the_shard_that_runs_its_test(self) -> None:
        floors = self.inventory["feature_lane_test_floors"]
        self.assertTrue(floors)
        held: collections.Counter[str] = collections.Counter()
        for shard in ALL_SHARDS:
            slice_ = shards.shard_floors(self.inventory, self.sizes, shard)
            executed = set(
                shards.selection(self.inventory, self.sizes, "//:feature_lane_tests", shard)
            )
            self.assertLessEqual(set(slice_), executed)
            for label, floor in slice_.items():
                self.assertEqual(floors[label], floor)
            held.update(slice_)
        self.assertEqual(collections.Counter(floors), held)

    def test_a_lane_is_not_split_between_shards(self) -> None:
        lanes = self.inventory["feature_lanes"]
        membership = collections.Counter(
            label for labels in lanes.values() for label in labels
        )
        for lane, labels in sorted(lanes.items()):
            own = {self.owner[label] for label in labels if membership[label] == 1}
            with self.subTest(lane=lane):
                self.assertLessEqual(len(own), 1)

    def test_no_shard_outweighs_the_heaviest_lane_by_much(self) -> None:
        # Lanes are atomic, so the heaviest one bounds the best possible
        # balance; a shard far above both that and an even share means the
        # placement stopped balancing.
        weight = shards.weights(self.inventory, self.sizes)
        total = sum(weight.values())
        heaviest_lane = max(
            sum(weight[label] for label in labels)
            for labels in self.inventory["feature_lanes"].values()
        )
        bound = max(heaviest_lane, total / shards.SHARDS) * 1.25
        for shard in ALL_SHARDS:
            load = sum(weight[label] for label, owner in self.owner.items() if owner == shard)
            with self.subTest(shard=shard):
                self.assertGreater(load, 0)
                self.assertLessEqual(load, bound)

    def test_measured_crates_weigh_their_compile_reservation(self) -> None:
        weight = shards.weights(self.inventory, self.sizes)
        self.assertGreater(
            sum(value != shards.DEFAULT_WEIGHT for value in weight.values()),
            len(weight) // 4,
            "the action-size table no longer matches the lane targets",
        )


class PlacementTests(unittest.TestCase):
    def test_a_shared_target_belongs_to_the_first_lane_placed(self) -> None:
        inventory = synthetic(
            {"big": ["//a:1", "//a:2", "//a:shared"], "small": ["//a:shared", "//b:1"]}
        )
        owner = shards.partition(inventory, {}, 2)
        self.assertEqual({"//a:1": 1, "//a:2": 1, "//a:shared": 1, "//b:1": 2}, owner)

    def test_lanes_go_heaviest_first_to_the_lightest_shard(self) -> None:
        inventory = synthetic(
            {
                "a": [f"//a:{index}" for index in range(5)],
                "b": [f"//b:{index}" for index in range(3)],
                "c": [f"//c:{index}" for index in range(2)],
                "d": ["//d:0"],
            }
        )
        owner, placed = shards.placement(inventory, {}, 2)
        self.assertEqual({"a": 1, "b": 2, "c": 2, "d": 1}, placed)
        self.assertEqual(6, sum(shard == 1 for shard in owner.values()))

    def test_the_partition_ignores_table_order(self) -> None:
        lanes = {
            "a": ["//a:2", "//a:1"],
            "b": ["//b:1", "//a:1"],
            "c": ["//c:1"],
        }
        reordered = {name: list(reversed(lanes[name])) for name in reversed(lanes)}
        self.assertEqual(
            shards.partition(synthetic(lanes), {}, 2),
            shards.partition(synthetic(reordered), {}, 2),
        )

    def test_a_measured_crate_outweighs_an_unmeasured_one(self) -> None:
        inventory = synthetic({"heavy": ["//p:lib__fv_1"], "light": ["//q:a", "//q:b"]})
        inventory["packages"] = [
            {"package": "pkg", "targets": [{"label": "//p:lib", "cargo": "lib"}]}
        ]
        sizes = {"pkg/lib": {"cpu_count": 2, "memory_kb": 4718592}}
        weight = shards.weights(inventory, sizes)
        self.assertEqual(2 * 4718592, weight["//p:lib__fv_1"])
        self.assertEqual(shards.DEFAULT_WEIGHT, weight["//q:a"])
        self.assertEqual({"heavy": 1, "light": 2}, shards.placement(inventory, sizes, 2)[1])

    def test_a_target_outside_every_lane_is_an_error(self) -> None:
        inventory = synthetic({"a": ["//a:1"]}, clippy=["//stray:1"])
        with self.assertRaisesRegex(ValueError, "//stray:1"):
            shards.partition(inventory, {})

    def test_an_unknown_group_or_shard_is_an_error(self) -> None:
        with self.assertRaisesRegex(ValueError, "unknown feature-lane group"):
            shards.selection(synthetic({"a": ["//a:1"]}), {}, "//:workspace_compile", 1)
        for value in ("0", str(shards.SHARDS + 1), "1/4", ""):
            with self.subTest(value=value), self.assertRaises(ValueError):
                shards.parse_shard(value)


class CommandTests(unittest.TestCase):
    def run_script(self, *argv: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(SCRIPT), *argv],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_the_command_receives_the_shards_labels(self) -> None:
        inventory, sizes = shards.load()
        group = "//:feature_lane_compile"
        result = self.run_script("1", group, "--", "printf", "%s\\n", "build")
        self.assertEqual(0, result.returncode, result.stderr)
        lines = result.stdout.splitlines()
        self.assertIn("shard 1/", lines[0])
        self.assertEqual(["build", *shards.selection(inventory, sizes, group, 1)], lines[1:])

    def test_a_shard_that_owns_nothing_of_a_group_skips_the_command(self) -> None:
        inventory, sizes = shards.load()
        group = "//:feature_lane_clippy"
        empty = [
            shard for shard in ALL_SHARDS if not shards.selection(inventory, sizes, group, shard)
        ]
        self.assertTrue(empty, "every shard lints; this case needs a synthetic group")
        result = self.run_script(str(empty[0]), group, "--", "false")
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("owns no //:feature_lane_clippy targets", result.stdout)

    def test_the_commands_failure_is_the_steps_failure(self) -> None:
        result = self.run_script("1", "//:feature_lane_compile", "--", "false")
        self.assertEqual(1, result.returncode)

    def test_a_bad_invocation_fails(self) -> None:
        for argv in (
            (),
            ("1", "//:feature_lane_compile"),
            ("0", "//:feature_lane_compile", "--", "true"),
            ("1", "//:workspace_compile", "--", "true"),
        ):
            with self.subTest(argv=argv):
                self.assertNotEqual(0, self.run_script(*argv).returncode)

    def test_the_floor_check_builds_only_the_shards_floors(self) -> None:
        inventory, sizes = shards.load()
        holder = next(
            shard for shard in ALL_SHARDS if shards.shard_floors(inventory, sizes, shard)
        )
        expected = shards.shard_floors(inventory, sizes, holder)
        built: list[list[str]] = []

        def record(argv, **_kwargs):
            built.append(list(argv))
            raise subprocess.CalledProcessError(1, argv)

        with mock.patch.object(floors_check.subprocess, "run", side_effect=record):
            with self.assertRaises(subprocess.CalledProcessError):
                floors_check.main(["--shard", str(holder)])
        self.assertEqual(sorted(expected), [arg for arg in built[0] if arg.startswith("//")])

    def test_the_floor_check_passes_a_shard_without_floors(self) -> None:
        with mock.patch.object(
            floors_check.feature_lane_shards, "shard_floors", return_value={}
        ), mock.patch.object(
            floors_check.subprocess, "run", side_effect=AssertionError("built")
        ), mock.patch("builtins.print"):
            self.assertEqual(0, floors_check.main(["--shard", "1"]))
        for argv in (["--shard"], ["--shard", "0"], ["1"]):
            with self.subTest(argv=argv), self.assertRaises(SystemExit) as raised:
                floors_check.main(argv)
            self.assertNotIn(raised.exception.code, (0, None))


class WorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.jobs = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        cls.job = cls.jobs["feature-lanes"]
        cls.steps = {step.get("name"): step for step in cls.job["steps"]}

    @staticmethod
    def flat(run: str) -> str:
        return " ".join(run.replace("\\\n", " ").split())

    def test_the_matrix_runs_every_shard_to_completion(self) -> None:
        strategy = self.job["strategy"]
        self.assertEqual(list(ALL_SHARDS), strategy["matrix"]["shard"])
        self.assertEqual(["shard"], list(strategy["matrix"]))
        self.assertIs(False, strategy["fail-fast"])
        self.assertEqual("${{ matrix.shard }}", self.job["env"]["FEATURE_LANE_SHARD"])
        self.assertIn(f"${{{{ matrix.shard }}}}/{shards.SHARDS})", self.job["name"])
        self.assertLessEqual(self.job["timeout-minutes"], 40)

    def test_each_step_selects_its_aggregate_for_the_shard(self) -> None:
        selector = 'python3 scripts/ci/feature_lane_shards.py "$FEATURE_LANE_SHARD" '
        compile_run = self.flat(self.steps["Compile every feature lane"]["run"])
        for command in (
            selector + "//:feature_lane_compile -- scripts/hermetic-build.sh build --jobs",
            selector + "//:feature_lane_clippy -- scripts/hermetic-build.sh clippy --jobs",
        ):
            with self.subTest(command=command):
                self.assertEqual(1, compile_run.count(command))
        self.assertEqual(2, compile_run.count("scripts/hermetic-build.sh"))
        self.assertEqual(
            selector + "//:feature_lane_tests -- scripts/ci/buck2-test.sh feature-lanes",
            self.flat(self.steps["Run the executable feature lanes"]["run"]),
        )
        self.assertEqual(
            'python3 scripts/ci/check_feature_lane_test_floors.py --shard "$FEATURE_LANE_SHARD"',
            self.steps["Hold the feature-lane test-case floors"]["run"],
        )

    def test_each_shard_uploads_under_its_own_name(self) -> None:
        for name in ("Upload failing test logs", "Upload feature-lane build events"):
            with self.subTest(step=name):
                self.assertIn("${{ matrix.shard }}", self.steps[name]["with"]["name"])

    def test_a_failed_shard_fails_the_conclusion(self) -> None:
        # `needs.feature-lanes.result` is the matrix's one result: failure or
        # cancelled if any shard is, skipped only when the job's own `if` is
        # false. Anything that lets a red shard read green would break that.
        self.assertNotIn("continue-on-error", self.job)
        for step in self.job["steps"]:
            self.assertNotIn("continue-on-error", step)
        conclusion = self.jobs["ci-conclusion"]
        self.assertIn("feature-lanes", conclusion["needs"])
        self.assertNotIn("matrix", " ".join(self.job["if"].split()))


if __name__ == "__main__":
    unittest.main()
