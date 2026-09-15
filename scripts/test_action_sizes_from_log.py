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


class CpuFloorTest(unittest.TestCase):
    """Declared floors override the measured cap, row or no row.

    A measured average can never exceed the cap the sample ran under, so a
    saturated CPU-bound compile reads as "sized correctly" and the table can
    never raise it. The actions on the critical-path chain therefore declare a
    floor in the generator instead.
    """

    def test_a_measured_row_is_raised_to_its_floor(self) -> None:
        # lash_core/lib is measured at two cores and saturated there.
        self.assertEqual(generator.ACTION_SIZES["lash_core/lib"]["cpu_count"], 2)
        self.assertEqual(
            generator.exec_properties("lash_core", "lib")["cpu_count"],
            str(generator.CPU_FLOORS["lash_core/lib"]),
        )

    def test_a_floored_key_with_no_measured_row_is_still_emitted(self) -> None:
        # The facade rlib sits between the lash-core rlib and the test binaries
        # on the critical path and has never produced a row.
        self.assertNotIn("lash/lib", generator.ACTION_SIZES)
        properties = generator.exec_properties("lash", "lib")
        self.assertEqual(properties["cpu_count"], str(generator.CPU_FLOORS["lash/lib"]))
        # An unmeasured floor may not shrink the cgroup it widens the cap of.
        self.assertEqual(properties["memory_kb"], str(generator.CI_DEFAULT_MEMORY_KB))

    def test_a_floor_never_lowers_a_measured_row(self) -> None:
        for key, floor in generator.CPU_FLOORS.items():
            entry = generator.ACTION_SIZES.get(key) or {}
            with self.subTest(key=key):
                self.assertGreaterEqual(floor, entry.get("cpu_count", 0))
                self.assertLessEqual(floor, sizes.MAX_CPU_COUNT)

    def test_every_floored_key_names_a_generated_target(self) -> None:
        emitted = set()
        pattern = re.compile(r'crate_name = "([a-z0-9_]+)"')
        for path in sorted(ROOT.rglob("BUILD.bazel")):
            text = path.read_text(encoding="utf-8")
            if "@generated by tools/bazel/generate_build_files.py" not in text:
                continue
            for block in text.split("\n\n"):
                match = pattern.search(block)
                if not match:
                    continue
                if block.startswith("lash_rust_library("):
                    emitted.add(f"{match.group(1)}/lib")
                elif block.startswith(("lash_rust_unit_test(", "lash_rust_integration_test(")):
                    emitted.add(f"{match.group(1)}/test")
                elif block.startswith("lash_rust_binary("):
                    emitted.add(f"{match.group(1)}/bin")
        for key in generator.CPU_FLOORS:
            with self.subTest(key=key):
                self.assertIn(key, emitted, "a floor that names no target is dead")


class CiDefaultsTest(unittest.TestCase):
    """No emitted request may ask the pool for less than CI's own default.

    CI never passes `--config=shared`: `.github/actions/bazel-shared-cache`
    composes its own flag list, and it sets `cpu_count=4` / `memory_kb=4 GiB`
    as the remote defaults. An emitted `exec_properties` pair replaces both, so
    a row that states one or two cores is not "as measured" on CI, it is a cap
    below what an unsized target gets.
    """

    ACTION = ROOT / ".github/actions/bazel-shared-cache/action.yml"

    def test_the_generator_matches_the_ci_action(self) -> None:
        action = self.ACTION.read_text(encoding="utf-8")
        self.assertIn(
            f"--remote_default_exec_properties=cpu_count={generator.CI_DEFAULT_CPU_COUNT}",
            action,
        )
        self.assertIn(
            f"--remote_default_exec_properties=memory_kb={generator.CI_DEFAULT_MEMORY_KB}",
            action,
        )

    def test_ci_still_composes_its_own_flags_rather_than_config_shared(self) -> None:
        # The day CI starts passing `--config=shared`, the repository defaults
        # in `.bazelrc` become the CI defaults too and this floor is the wrong
        # shape. Fail here rather than silently over-reserving.
        self.assertNotIn("--config=shared", self.ACTION.read_text(encoding="utf-8"))

    def test_no_generated_target_asks_for_less_than_the_ci_default(self) -> None:
        pattern = re.compile(r'exec_properties = \{"cpu_count": "(\d+)", "memory_kb"')
        seen = 0
        for path in sorted(ROOT.rglob("BUILD.bazel")):
            text = path.read_text(encoding="utf-8")
            if "@generated by tools/bazel/generate_build_files.py" not in text:
                continue
            for match in pattern.finditer(text):
                seen += 1
                self.assertGreaterEqual(
                    int(match.group(1)),
                    generator.CI_DEFAULT_CPU_COUNT,
                    f"{path}: a sized target below CI's own default cap",
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
