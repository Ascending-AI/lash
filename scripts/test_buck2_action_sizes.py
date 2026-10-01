#!/usr/bin/env python3
"""Preserve compile and test-run sizing contracts from worker usage records."""

from __future__ import annotations

import json
import pathlib
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools/buck2"))
import action_sizes_from_log as sizes  # noqa: E402

GIB = 1024**3
CRATES = {("lash-internal-core", "lash_core"), ("lash-sim", "logical_turn")}
TEST_LABELS = {"//crates/lash-core:runtime_turns__test", "//crates/lash-core:test_batch"}

def record(
    pkg: str = "lash-internal-core",
    crate: str = "lash_core",
    cores: float = 1.0,
    peak_bytes: int = GIB,
    wall_ms: int = 10_000,
    requested_cpu: str = "4",
    tool: str = "python3",
    exit_status: str = "0",
) -> str:
    cpu_usec = int(cores * wall_ms * 1000)
    return "\t".join(
        [
            "1789424023",
            f"crate={crate}",
            f"pkg={pkg}",
            "kind=-",
            f"tool={tool}",
            f"peak_bytes={peak_bytes}",
            f"cpu_usec={cpu_usec}",
            f"wall_ms={wall_ms}",
            f"exit={exit_status}",
            "requested_kb=4194304",
            f"requested_cpu={requested_cpu}",
        ]
    )


def table(lines: list[str]) -> dict:
    return sizes.table(sizes.collect(lines, CRATES))


class CollectTest(unittest.TestCase):
    def test_only_lash_packages_are_measured(self) -> None:
        # The pool is shared: another repository's crate, even one with a Lash
        # crate's name, never reaches this table.
        lines = [record(cores=3.0)] * 20
        lines += [record(pkg="control-plane", crate="control_plane", cores=7.0)] * 50
        lines += [record(pkg="other-repo", crate="lash_core", cores=7.0)] * 50
        measured = sizes.collect(lines, CRATES)
        self.assertEqual(sorted(measured), ["lash-internal-core/lash_core"])
        self.assertEqual(len(measured["lash-internal-core/lash_core"].records), 20)

    def test_only_successful_compiles_of_a_second_or_more_count(self) -> None:
        lines = [
            record(tool="test-setup.sh"),
            record(tool="generate-xml.sh"),
            record(exit_status="137"),
            record(wall_ms=999),
            "torn line",
            "1789424023\tcrate=lash_core\tno-separator",
        ]
        self.assertEqual(sizes.collect(lines, CRATES), {})

    def test_first_party_crates_come_from_workspace_targets(self) -> None:
        metadata = {
            "packages": [
                {
                    "name": "lash-sim",
                    "targets": [
                        {"name": "lash_sim", "kind": ["lib"]},
                        {"name": "logical-turn", "kind": ["test"]},
                        {"name": "build-script-build", "kind": ["custom-build"]},
                    ],
                }
            ]
        }
        self.assertEqual(
            sizes.first_party_crates(metadata),
            {("lash-sim", "lash_sim"), ("lash-sim", "logical_turn")},
        )


class TableTest(unittest.TestCase):
    def test_cpu_is_the_p95_rounded_up_past_the_tolerance(self) -> None:
        # p95 of 19 x 1.0 and 1 x 2.5 cores is 1.0 (nearest rank), so 1 CPU;
        # memory keeps the row.
        lines = [record(cores=1.0, peak_bytes=2 * GIB)] * 19 + [record(cores=2.5)]
        self.assertEqual(table(lines)["lash-internal-core/lash_core"]["cpu_count"], 1)
        # 2.15 cores at p95 is within the tolerance of 2.
        lines = [record(cores=2.15)] * 20
        self.assertEqual(table(lines)["lash-internal-core/lash_core"]["cpu_count"], 2)
        # 2.25 is not.
        lines = [record(cores=2.25)] * 20
        self.assertEqual(table(lines)["lash-internal-core/lash_core"]["cpu_count"], 3)

    def test_cpu_is_capped_at_eight(self) -> None:
        lines = [record(cores=14.0)] * 20
        self.assertEqual(table(lines)["lash-internal-core/lash_core"]["cpu_count"], 8)
        self.assertEqual(sizes.MAX_CPU_COUNT, 8)

    def test_one_cpu_samples_are_left_out_when_larger_requests_exist(self) -> None:
        # Under a 1-CPU quota an action cannot show that it wants more.
        lines = [record(cores=1.0, requested_cpu="1")] * 200
        lines += [record(cores=2.0, requested_cpu="4")] * 20
        entry = table(lines)["lash-internal-core/lash_core"]
        self.assertEqual(entry["cpu_count"], 2)
        self.assertEqual(entry["samples"], 220)

    def test_one_cpu_samples_are_used_when_they_are_all_there_is(self) -> None:
        lines = [record(cores=1.6, requested_cpu="1")] * 20
        self.assertEqual(table(lines)["lash-internal-core/lash_core"]["cpu_count"], 2)

    def test_memory_follows_the_worst_peak_with_margin(self) -> None:
        lines = [record(cores=2.0, peak_bytes=GIB)] * 19
        lines += [record(cores=2.0, peak_bytes=3 * GIB)]
        entry = table(lines)["lash-internal-core/lash_core"]
        self.assertEqual(entry["peak_bytes"], 3 * GIB)
        # 3 GiB x 1.5 = 4.5 GiB, already a multiple of 512 MiB.
        self.assertEqual(entry["memory_kb"], 4718592)

    def test_memory_never_goes_below_the_default(self) -> None:
        lines = [record(cores=2.0, peak_bytes=GIB // 4)] * 20
        self.assertEqual(
            table(lines)["lash-internal-core/lash_core"]["memory_kb"],
            sizes.DEFAULT_MEMORY_KB,
        )

    def test_too_few_samples_make_no_row(self) -> None:
        lines = [record(pkg="lash-sim", crate="logical_turn", cores=4.0)] * (
            sizes.MIN_SAMPLES - 1
        )
        self.assertNotIn("lash-sim/logical_turn", table(lines))

    def test_a_row_at_the_defaults_is_dropped(self) -> None:
        lines = [record(pkg="lash-sim", crate="logical_turn", cores=1.0)] * 20
        self.assertNotIn("lash-sim/logical_turn", table(lines))

    def test_the_minimum_floor_keeps_and_roughly_sizes_a_row(self) -> None:
        saved = dict(sizes.MINIMUM_MEMORY_KB)
        try:
            sizes.MINIMUM_MEMORY_KB["lash-internal-core/lash_core"] = 5 * 1024 * 1024
            # The formula lands at 1.5 GiB from a 1 GiB peak; the floor wins.
            lines = [record(cores=2.0, peak_bytes=GIB)] * 20
            entry = table(lines)["lash-internal-core/lash_core"]
            self.assertEqual(entry["memory_kb"], 5242880)
            self.assertEqual(entry["cpu_count"], 2)
            # Too few samples to measure still yields the floor, not absence.
            entry = table([record(cores=1.0, peak_bytes=GIB)] * 3)[
                "lash-internal-core/lash_core"
            ]
            self.assertEqual(entry["memory_kb"], 5242880)
            self.assertEqual(entry["samples"], 3)
        finally:
            sizes.MINIMUM_MEMORY_KB.clear()
            sizes.MINIMUM_MEMORY_KB.update(saved)

    def test_a_measured_need_above_the_minimum_wins(self) -> None:
        saved = dict(sizes.MINIMUM_MEMORY_KB)
        try:
            sizes.MINIMUM_MEMORY_KB["lash-internal-core/lash_core"] = 2 * 1024 * 1024
            lines = [record(cores=2.0, peak_bytes=3 * GIB)] * 20
            self.assertEqual(
                table(lines)["lash-internal-core/lash_core"]["memory_kb"], 4718592
            )
        finally:
            sizes.MINIMUM_MEMORY_KB.clear()
            sizes.MINIMUM_MEMORY_KB.update(saved)

    def test_rendered_table_is_sorted_and_newline_terminated(self) -> None:
        lines = [record(cores=2.0)] * 20
        lines += [record(pkg="lash-sim", crate="logical_turn", cores=2.0)] * 20
        rendered = sizes.render(table(lines))
        self.assertTrue(rendered.endswith("}\n"))
        self.assertEqual(list(json.loads(rendered)), sorted(json.loads(rendered)))

    def test_the_checked_in_table_matches_the_documented_shape(self) -> None:
        checked_in = json.loads(
            (ROOT / "tools/buck2/action-sizes.json").read_text(encoding="utf-8")
        )
        inventory = json.loads(
            (ROOT / "tools/buck2/target-inventory.json").read_text(encoding="utf-8")
        )
        crates = {
            f"{package['package']}/{target['cargo'].replace('-', '_')}"
            for package in inventory["packages"]
            for target in package["targets"]
            if target.get("cargo")
        }
        self.assertTrue(checked_in)
        for key, entry in checked_in.items():
            with self.subTest(key=key):
                self.assertIn(key, crates, "a row that names no first-party crate is dead")
                self.assertEqual(
                    sorted(entry),
                    ["cpu_count", "memory_kb", "p95_cores", "peak_bytes", "samples"],
                )
                self.assertTrue(
                    entry["memory_kb"] > sizes.DEFAULT_MEMORY_KB
                    or entry["cpu_count"] > sizes.DEFAULT_CPU_COUNT,
                    "an entry that asks for no more than the defaults is noise",
                )
                self.assertEqual(entry["cpu_count"], sizes.cpu_count_for(entry["p95_cores"]))
                self.assertEqual(
                    entry["memory_kb"],
                    max(
                        sizes.memory_kb_for(entry["peak_bytes"]),
                        sizes.MINIMUM_MEMORY_KB.get(key, 0),
                    ),
                )
                self.assertTrue(
                    entry["samples"] >= sizes.MIN_SAMPLES
                    or key in sizes.MINIMUM_MEMORY_KB,
                    "an unmeasured row survives only on an explicit minimum",
                )


def run_record(
    label: str = "//crates/lash-core:runtime_turns__test",
    role: str = "run",
    cores: float = 1.0,
    peak_bytes: int = 100 * 1024 * 1024,
    wall_ms: int = 4_000,
    exit_status: str = "0",
    requested_cpu: str = "4",
) -> str:
    """A test action's usage record, as the supervisor writes it since kiln#28."""
    return "\t".join(
        [
            "1790160000",
            "crate=-",
            "pkg=-",
            "kind=-",
            "tool=test-setup.sh",
            f"peak_bytes={peak_bytes}",
            f"cpu_usec={int(cores * wall_ms * 1000)}",
            f"wall_ms={wall_ms}",
            f"exit={exit_status}",
            "requested_kb=4194304",
            f"requested_cpu={requested_cpu}",
            f"test={role}:-:{label}",
        ]
    )


def run_table(lines: list[str]) -> dict:
    return sizes.test_run_table(sizes.collect_test_runs(lines, TEST_LABELS))


class TestRunTableTest(unittest.TestCase):
    """Test runs are sized per target label from the `test` field of the log."""

    def test_only_successful_runs_of_generated_labels_count(self) -> None:
        lines = [run_record()] * 3
        lines += [run_record(role="xml")] * 5
        lines += [run_record(exit_status="1")] * 5
        lines += [run_record(label="//crates/hirsel-proto:hirsel-proto__unit_test")] * 5
        # A compile record, and one written before the field existed.
        lines += [record()] * 5
        lines += [run_record().rsplit("\t", 1)[0]] * 5
        table_ = run_table(lines)
        self.assertEqual(
            list(table_),
            ["//crates/lash-core:runtime_turns__test"]
            + sorted(sizes.TEST_RUN_MINIMUM_MEMORY_KB),
        )
        self.assertEqual(table_["//crates/lash-core:runtime_turns__test"]["samples"], 3)

    def test_a_label_containing_colons_is_read_whole(self) -> None:
        label = "//crates/lash-core:test_batch"
        self.assertIn(label, run_table([run_record(label=label)] * 3))

    def test_root_cell_runs_share_the_owned_inventory_row(self) -> None:
        label = "//crates/lash-core:runtime_turns__test"
        lines = [run_record(label="root" + label)] * 3
        lines += [run_record(label="other" + label)] * 3
        measured = sizes.collect_test_runs(lines, TEST_LABELS)
        self.assertEqual(list(measured), [label])
        self.assertEqual(measured[label].count, 3)

    def test_three_samples_make_a_row_and_two_do_not(self) -> None:
        self.assertEqual(sizes.TEST_MIN_SAMPLES, 3)
        table_ = run_table([run_record()] * 2)
        self.assertNotIn("//crates/lash-core:runtime_turns__test", table_)
        self.assertEqual(list(table_), sorted(sizes.TEST_RUN_MINIMUM_MEMORY_KB))
        table_ = run_table([run_record()] * 3)
        self.assertEqual(
            list(table_),
            ["//crates/lash-core:runtime_turns__test"]
            + sorted(sizes.TEST_RUN_MINIMUM_MEMORY_KB),
        )

    def test_memory_is_the_peak_with_margin_and_a_one_gib_floor(self) -> None:
        entry = run_table([run_record(peak_bytes=100 * 1024 * 1024)] * 3)
        self.assertEqual(entry["//crates/lash-core:runtime_turns__test"]["memory_kb"], 1048576)
        # 7.45 GiB x 1.5 = 11.2 GiB, rounded up to 11.5 GiB.
        entry = run_table([run_record(peak_bytes=7_999_586_304)] * 3)
        self.assertEqual(entry["//crates/lash-core:runtime_turns__test"]["memory_kb"], 12058624)

    def test_the_minimum_floor_keeps_and_roughly_sizes_a_row(self) -> None:
        saved = dict(sizes.TEST_RUN_MINIMUM_MEMORY_KB)
        try:
            sizes.TEST_RUN_MINIMUM_MEMORY_KB["//crates/lash-core:runtime_turns__test"] = (
                5 * 1024 * 1024
            )
            # The formula lands at 1 GiB from a 100 MiB peak; the floor wins.
            entry = run_table([run_record(peak_bytes=100 * 1024 * 1024)] * 3)[
                "//crates/lash-core:runtime_turns__test"
            ]
            self.assertEqual(entry["memory_kb"], 5242880)
            # Too few samples to measure still yields the floor, not absence.
            entry = run_table([run_record(peak_bytes=100 * 1024 * 1024)])[
                "//crates/lash-core:runtime_turns__test"
            ]
            self.assertEqual(entry["memory_kb"], 5242880)
            self.assertEqual(entry["samples"], 1)
            # A measured need above the floor still wins.
            entry = run_table([run_record(peak_bytes=5 * GIB)] * 3)[
                "//crates/lash-core:runtime_turns__test"
            ]
            self.assertEqual(entry["memory_kb"], 7864320)
            # A feature-variant label carries the floor of its base label.
            variant = "//crates/lash-core:runtime_turns__test__fv_0123abcd"
            lines = [run_record(label=variant, peak_bytes=100 * 1024 * 1024)] * 3
            entry = sizes.test_run_table(
                sizes.collect_test_runs(lines, TEST_LABELS | {variant})
            )[variant]
            self.assertEqual(entry["memory_kb"], 5242880)
        finally:
            sizes.TEST_RUN_MINIMUM_MEMORY_KB.clear()
            sizes.TEST_RUN_MINIMUM_MEMORY_KB.update(saved)

    def test_cpu_is_the_compile_p95_rule(self) -> None:
        entry = run_table([run_record(cores=2.15)] * 3)
        self.assertEqual(entry["//crates/lash-core:runtime_turns__test"]["cpu_count"], 2)
        entry = run_table([run_record(cores=2.25)] * 3)
        self.assertEqual(entry["//crates/lash-core:runtime_turns__test"]["cpu_count"], 3)
        entry = run_table([run_record(cores=20.0)] * 3)
        self.assertEqual(entry["//crates/lash-core:runtime_turns__test"]["cpu_count"], 8)

    def test_sub_second_runs_count_as_samples_but_not_as_cpu_evidence(self) -> None:
        entry = run_table([run_record(cores=3.0, wall_ms=400)] * 3)[
            "//crates/lash-core:runtime_turns__test"
        ]
        self.assertEqual((entry["samples"], entry["cpu_count"], entry["p95_cores"]), (3, 1, 0.0))
        lines = [run_record(cores=3.0, wall_ms=400)] * 3 + [run_record(cores=2.5)]
        entry = run_table(lines)["//crates/lash-core:runtime_turns__test"]
        self.assertEqual((entry["samples"], entry["cpu_count"]), (4, 3))

    def test_the_checked_in_table_names_generated_tests_only(self) -> None:
        checked_in = json.loads(
            (ROOT / "tools/buck2/test-run-sizes.json").read_text(encoding="utf-8")
        )
        inventory = json.loads(
            (ROOT / "tools/buck2/target-inventory.json").read_text(encoding="utf-8")
        )
        labels = sizes.test_labels(inventory)
        self.assertTrue(checked_in)
        for label, entry in checked_in.items():
            with self.subTest(label=label):
                self.assertIn(label, labels)
                self.assertEqual(
                    sorted(entry),
                    ["cpu_count", "memory_kb", "p95_cores", "peak_bytes", "samples"],
                )
                self.assertTrue(
                    entry["samples"] >= sizes.TEST_MIN_SAMPLES
                    or label in sizes.TEST_RUN_MINIMUM_MEMORY_KB,
                    "an unmeasured row survives only on an explicit minimum",
                )
                self.assertGreaterEqual(
                    entry["memory_kb"],
                    max(
                        sizes.TEST_MEMORY_FLOOR_KB,
                        sizes.TEST_RUN_MINIMUM_MEMORY_KB.get(
                            label,
                            sizes.TEST_RUN_MINIMUM_MEMORY_KB.get(
                                label.split("__fv_", 1)[0], 0
                            ),
                        ),
                    ),
                )
                self.assertTrue(1 <= entry["cpu_count"] <= sizes.MAX_CPU_COUNT)


if __name__ == "__main__":
    unittest.main()
