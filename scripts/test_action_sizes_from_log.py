#!/usr/bin/env python3
"""Unit tests for `tools/bazel/action_sizes_from_log.py`."""

from __future__ import annotations

import json
import pathlib
import re
import sys
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools/bazel"))

import action_sizes_from_log as sizes  # noqa: E402
import generate_build_files as generator  # noqa: E402


FIXTURE = ROOT / "tools/bazel/testdata/rustc-usage-sample.txt"


def table_from_fixture() -> dict:
    return sizes.table(sizes.parse_log(FIXTURE.read_text(encoding="utf-8").splitlines()))


class TableTest(unittest.TestCase):
    def test_peak_memory_is_the_loudest_sample_with_margin(self) -> None:
        entry = table_from_fixture()["lash_core/lib"]
        # Two samples, 1 GiB and 2 GiB; the request follows the 2 GiB one.
        self.assertEqual(entry["peak_bytes"], 2097152 * 1024)
        self.assertEqual(entry["samples"], 2)
        self.assertEqual(entry["memory_kb"], 3145728)

    def test_memory_requests_are_multiples_of_512_mib(self) -> None:
        for key, entry in table_from_fixture().items():
            with self.subTest(key=key):
                self.assertEqual(entry["memory_kb"] % (512 * 1024), 0)
                self.assertGreaterEqual(entry["memory_kb"], sizes.DEFAULT_MEMORY_KB)

    def test_cpu_request_is_cpu_seconds_over_wall_seconds(self) -> None:
        # 200 s user + 20 s sys over 300 s wall is one busy core, not three.
        self.assertEqual(table_from_fixture()["lash_core/test"]["cpu_count"], 1)
        # 9.6 + 0.4 over 2.5 s is exactly four.
        self.assertEqual(table_from_fixture()["serial_bin/bin"]["cpu_count"], 4)

    def test_cpu_request_is_capped(self) -> None:
        # 300 + 60 over 20 s is eighteen busy cores; the pool's cap is eight.
        self.assertEqual(table_from_fixture()["wide_test/test"]["cpu_count"], 8)
        self.assertEqual(sizes.MAX_CPU_COUNT, 8)

    def test_entries_at_or_below_the_defaults_are_dropped(self) -> None:
        self.assertNotIn("tiny_helper/lib", table_from_fixture())

    def test_the_shared_build_script_crate_name_is_never_keyed(self) -> None:
        # Loud enough to be kept on the numbers, and still dropped: every
        # package's build.rs compiles under this one name, and the compile is
        # not a target `cargo_build_script` can size.
        measured = sizes.parse_log(
            ["crate=build_script_build kind=build-script 4194304 40.0 4.0 10.0"]
        )
        self.assertGreater(measured["build_script_build/build-script"].peak_rss_kb, 0)
        self.assertEqual(sizes.table(measured), {})

    def test_a_kept_entry_never_lowers_the_memory_default(self) -> None:
        # Kept for its CPU request; its memory request must still be the
        # default, or the entry would shrink the cgroup the action runs in.
        self.assertEqual(
            table_from_fixture()["wide_test/test"]["memory_kb"],
            sizes.DEFAULT_MEMORY_KB,
        )

    def test_zero_wall_time_does_not_divide_by_zero(self) -> None:
        self.assertEqual(sizes.cpu_count_for(0.1, 0.0, 0.0), 1)

    def test_rendered_table_is_sorted_and_newline_terminated(self) -> None:
        rendered = sizes.render(table_from_fixture())
        self.assertTrue(rendered.endswith("}\n"))
        self.assertEqual(
            list(json.loads(rendered)), sorted(json.loads(rendered))
        )

    def test_the_checked_in_table_matches_the_documented_shape(self) -> None:
        table = json.loads(
            (ROOT / "tools/bazel/action-sizes.json").read_text(encoding="utf-8")
        )
        for key, entry in table.items():
            with self.subTest(key=key):
                crate, _, kind = key.rpartition("/")
                self.assertTrue(crate)
                self.assertIn(kind, sizes.KINDS)
                self.assertEqual(
                    sorted(entry), ["cpu_count", "memory_kb", "peak_bytes", "samples"]
                )
                self.assertTrue(
                    entry["memory_kb"] > sizes.DEFAULT_MEMORY_KB
                    or entry["cpu_count"] > sizes.DEFAULT_CPU_COUNT,
                    "an entry that asks for no more than the defaults is noise",
                )
                self.assertEqual(entry["memory_kb"] % (512 * 1024), 0)
                self.assertGreaterEqual(entry["memory_kb"], sizes.DEFAULT_MEMORY_KB)
                self.assertGreaterEqual(entry["cpu_count"], 1)
                self.assertLessEqual(entry["cpu_count"], sizes.MAX_CPU_COUNT)
                self.assertGreater(entry["samples"], 0)


class TestCpuFloorTest(unittest.TestCase):
    """Test kinds floor at four cores; every other kind stays as measured.

    A test action is a whole libtest binary running a tokio runtime and a
    store, so the crate's measured compile-time CPU does not describe it. At
    the repository default of one core the suite's own threads contend for
    that core and time-sensitive cases fail outright: CI run 34950810111 lost
    `//crates/lash-perf:lash-perf__unit_test` on a phase assertion and timed
    out `//crates/lash-sim:stack_policy__test`.
    """

    def test_a_test_kind_is_floored_even_with_no_measured_row(self) -> None:
        self.assertEqual(generator.TEST_CPU_FLOOR, 4)
        properties = generator.exec_properties("crate_with_no_row_at_all", "test")
        self.assertEqual(properties["cpu_count"], str(generator.TEST_CPU_FLOOR))
        self.assertEqual(properties["memory_kb"], str(generator.DEFAULT_MEMORY_KB))

    def test_a_measured_test_row_above_the_floor_is_left_alone(self) -> None:
        generator.ACTION_SIZES["floor_probe/test"] = {
            "cpu_count": 8,
            "memory_kb": generator.DEFAULT_MEMORY_KB,
        }
        try:
            self.assertEqual(
                generator.exec_properties("floor_probe", "test")["cpu_count"], "8"
            )
        finally:
            del generator.ACTION_SIZES["floor_probe/test"]

    def test_libs_bins_and_build_scripts_are_not_floored(self) -> None:
        for kind in ("lib", "bin", "build-script"):
            with self.subTest(kind=kind):
                self.assertEqual(
                    generator.exec_properties("crate_with_no_row_at_all", kind), {}
                )

    def test_every_generated_test_target_asks_for_the_floor(self) -> None:
        pattern = re.compile(r'exec_properties = \{"cpu_count": "(\d+)"')
        test_rules = ("lash_rust_unit_test(", "lash_rust_integration_test(")
        seen = 0
        for path in sorted(ROOT.rglob("BUILD.bazel")):
            text = path.read_text(encoding="utf-8")
            if "@generated by tools/bazel/generate_build_files.py" not in text:
                continue
            for block in text.split("\n\n"):
                if not block.startswith(test_rules):
                    continue
                match = pattern.search(block)
                self.assertIsNotNone(
                    match, f"{path}: a test target with no sized request"
                )
                seen += 1
                self.assertGreaterEqual(
                    int(match.group(1)),
                    generator.TEST_CPU_FLOOR,
                    f"{path}: a test target below the CPU floor",
                )
        self.assertGreater(seen, 0)


class DefaultsAgreeTest(unittest.TestCase):
    """The two defaults are written down in three places; they must match.

    `.bazelrc` is what the pool is actually asked for, and the other two decide
    which rows are worth keeping and what a kept row's other half should say. A
    drift between them silently either drops rows that do exceed the default or
    keeps rows that do not.
    """

    def test_bazelrc_and_the_tools_agree(self) -> None:
        bazelrc = (ROOT / ".bazelrc").read_text(encoding="utf-8")
        self.assertIn(
            f"--remote_default_exec_properties=cpu_count={sizes.DEFAULT_CPU_COUNT}\n",
            bazelrc,
        )
        self.assertIn(
            f"--remote_default_exec_properties=memory_kb={sizes.DEFAULT_MEMORY_KB}\n",
            bazelrc,
        )
        generator = (ROOT / "tools/bazel/generate_build_files.py").read_text(
            encoding="utf-8"
        )
        self.assertIn(f"DEFAULT_MEMORY_KB = {sizes.DEFAULT_MEMORY_KB}\n", generator)
        self.assertIn(f"DEFAULT_CPU_COUNT = {sizes.DEFAULT_CPU_COUNT}\n", generator)


class ParseTest(unittest.TestCase):
    def test_short_line_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            sizes.parse_log(["crate=x kind=lib 1024 0.0 0.0"])

    def test_unknown_kind_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            sizes.parse_log(["crate=x kind=doc 1024 0.0 0.0 1.0"])

    def test_blank_lines_are_ignored(self) -> None:
        self.assertEqual(sizes.parse_log(["", "   "]), {})


if __name__ == "__main__":
    unittest.main()
