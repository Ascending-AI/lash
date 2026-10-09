#!/usr/bin/env python3
"""Sizing a library apart from its unit-test binary, and the rows in force."""

from __future__ import annotations

import json
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import action_categories_from_events as joined  # noqa: E402
import action_sizes_from_log as sizes  # noqa: E402

GIB = 1024**3
KEY = "lash-internal-core/lash_core"
INVENTORY = {
    "packages": [
        {
            "package": "lash-internal-core",
            "targets": [
                {"cargo": "lash_core", "kind": "lib", "label": "//crates/lash-core:lash-core"},
                {
                    "cargo": "lash_core",
                    "kind": "unit-test",
                    "label": "//crates/lash-core:lash-core__unit_test",
                },
                {
                    "cargo": "runtime_turns",
                    "kind": "test",
                    "label": "//crates/lash-core:runtime_turns__test",
                },
                {"kind": "custom-build", "label": "//crates/lash-core:build_script"},
            ],
        }
    ],
    "feature_lane_units": [
        {
            "kind": "lib",
            "label": "//crates/lash-core:lash-core__fv_0f5f2ec4",
            "package": "lash-internal-core",
        }
    ],
}


def usage(
    stamp: int = 1000,
    crate: str = "lash_core",
    category: str = "rustc",
    peak_bytes: int = GIB,
    anon_peak_bytes: int | str = "-",
    wall_ms: int = 10_000,
    cores: float = 1.0,
    requested_kb: int = 4194304,
    pkg: str = "lash-internal-core",
) -> str:
    return "\t".join(
        [
            str(stamp),
            f"crate={crate}",
            f"pkg={pkg}",
            "kind=-",
            "tool=python3",
            f"peak_bytes={peak_bytes}",
            f"cpu_usec={int(cores * wall_ms * 1000)}",
            f"wall_ms={wall_ms}",
            "exit=0",
            f"requested_kb={requested_kb}",
            "requested_cpu=1",
            "test=-",
            "op=0",
            f"category={category}",
            f"anon_peak_bytes={anon_peak_bytes}",
        ]
    )


def event(
    name: str = "lash-core",
    identifier: str = "rlib [pic]",
    category: str = "rustc",
    end: float = 1000.0,
    wall: float = 10.0,
    memory_kb: int = 4194304,
    cache_hit: bool = False,
    configuration: str = "prelude//platforms:default#a8ac7ec86a6bee54",
) -> str:
    start = end - wall
    action = {
        "key": {
            "owner": {
                "TargetLabel": {
                    "label": {"package": "root//crates/lash-core", "name": name},
                    "configuration": {"full_name": configuration},
                }
            }
        },
        "name": {"category": category, "identifier": identifier},
        "commands": [
            {
                "details": {
                    "command_kind": {
                        "command": {
                            "RemoteCommand": {
                                "cache_hit": cache_hit,
                                "details": {
                                    "platform": {
                                        "properties": [
                                            {"name": "memory_kb", "value": str(memory_kb)}
                                        ]
                                    }
                                },
                            }
                        }
                    },
                    "metadata": {
                        "start_time": [int(start), int((start % 1) * 1e9)],
                        "execution_time_us": int(wall * 1e6),
                    },
                }
            }
        ],
    }
    return json.dumps({"Event": {"data": {"SpanEnd": {"data": {"ActionExecution": action}}}}})


def labelled(events: list[str], lines: list[str]) -> list[tuple]:
    return [
        (key, kind, category, emit)
        for key, kind, category, emit, _optimized, _record in joined.labelled_records(
            list(joined.executed_actions(events)), lines, INVENTORY
        )
    ]


def samples(peak_bytes: int, count: int, requested_kb: int = 4194304) -> sizes.Samples:
    kept = sizes.Samples()
    for _ in range(count):
        kept.observe(1.0, peak_bytes, 1, requested_kb)
    return kept


class JoinTest(unittest.TestCase):
    def test_an_executed_action_takes_its_one_matching_record(self) -> None:
        events = [
            event(),
            event(name="lash-core__unit_test__rust_test", identifier="link [pic]", end=2000.0),
            event(name="lash-core__fv_0f5f2ec4", identifier="metadata [pic]", end=3000.0),
            event(name="lash-core", category="clippy", identifier="", end=4000.0),
        ]
        lines = [usage(), usage(stamp=2000), usage(stamp=3000), usage(stamp=4000, category="clippy")]
        self.assertEqual(
            labelled(events, lines),
            [
                (KEY, "lib", "rustc", "rlib"),
                (KEY, "unit-test", "rustc", "link"),
                (KEY, "lib", "rustc", "metadata"),
                (KEY, "lib", "clippy", ""),
            ],
        )

    def test_a_guess_is_not_a_match(self) -> None:
        # Two records that fit, a cache hit, another request, another crate,
        # another wall time, and a target the inventory does not name.
        self.assertEqual(labelled([event()], [usage(), usage(stamp=1001)]), [])
        self.assertEqual(labelled([event(cache_hit=True)], [usage()]), [])
        self.assertEqual(labelled([event()], [usage(requested_kb=1572864)]), [])
        self.assertEqual(labelled([event()], [usage(crate="runtime_turns")]), [])
        self.assertEqual(labelled([event()], [usage(wall_ms=12_000)]), [])
        self.assertEqual(labelled([event(name="build_script")], [usage()]), [])

    def test_one_record_labels_one_action(self) -> None:
        self.assertEqual(len(labelled([event(), event(end=1001.0)], [usage()])), 1)

    def test_only_a_library_and_its_unit_test_share_an_identity(self) -> None:
        self.assertEqual(joined.shared_identities(INVENTORY), {KEY})


class KindTableTest(unittest.TestCase):
    def test_kinds_are_collected_per_shared_identity(self) -> None:
        events = [event(), event(name="lash-core__unit_test__rust_test", end=2000.0)]
        lines = [usage(peak_bytes=GIB // 2, anon_peak_bytes=GIB), usage(stamp=2000, peak_bytes=3 * GIB)]
        records = list(
            joined.labelled_records(list(joined.executed_actions(events)), lines, INVENTORY)
        )
        measured = sizes.collect_kinds(records, {KEY})
        self.assertEqual(sorted(measured), [(KEY, "target"), (KEY, "test")])
        # The larger of the cgroup peak and the sampled anon peak.
        self.assertEqual(measured[(KEY, "target")].peak_bytes(), GIB)
        self.assertEqual(sizes.collect_kinds(records, set()), {})

    def test_a_measured_kind_leaves_the_crate_row_when_it_moves(self) -> None:
        crate_rows = {KEY: {"cpu_count": 1, "memory_kb": 4194304}}
        rows = sizes.kind_table(
            {(KEY, "target"): samples(GIB // 2, 20), (KEY, "test"): samples(3 * GIB, 20)},
            crate_rows,
        )
        # The library fits the default; its test binary still fits the crate row.
        self.assertEqual(list(rows[KEY]), ["target"])
        self.assertEqual(rows[KEY]["target"]["memory_kb"], sizes.DEFAULT_MEMORY_KB)
        self.assertEqual(rows[KEY]["target"]["samples"], 20)

    def test_too_few_samples_keep_the_crate_row(self) -> None:
        crate_rows = {KEY: {"cpu_count": 1, "memory_kb": 4194304}}
        self.assertEqual(sizes.kind_table({(KEY, "target"): samples(GIB // 2, 19)}, crate_rows), {})

    def test_a_kind_row_in_force_stays_until_it_really_moves(self) -> None:
        current = {KEY: {"target": {"cpu_count": 1, "memory_kb": 2097152, "samples": 30}}}
        # 1.4 GiB x 1.25 is 1.75 GiB: 256 MiB under the row in force.
        measured = {(KEY, "target"): samples(int(1.4 * GIB), 20, requested_kb=1048576)}
        rows = sizes.kind_table(measured, {}, current)
        self.assertEqual(rows[KEY]["target"]["memory_kb"], 2097152)
        self.assertEqual(rows[KEY]["target"]["samples"], 20)
        # An unmeasured kind keeps its row untouched.
        self.assertEqual(sizes.kind_table({}, {}, current), current)
        # A peak above the row in force moves it whatever the distance.
        measured = {(KEY, "target"): samples(int(2.1 * GIB), 20, requested_kb=1048576)}
        self.assertEqual(sizes.kind_table(measured, {}, current)[KEY]["target"]["memory_kb"], 2883584)


class OptimizedTableTest(unittest.TestCase):
    def measured(self, peak_bytes: int, count: int) -> dict:
        return {(KEY, "target"): samples(peak_bytes, count, requested_kb=1048576)}

    def table(self, measured, dev=(1, 1572864), previous=None, current=None) -> dict:
        return sizes.optimized_table(
            measured, lambda key, kind: dev, lambda key, kind: previous or dev, current
        )

    def test_optimized_and_host_compiles_are_kept_apart_from_dev(self) -> None:
        events = [
            event(),
            event(end=2000.0, configuration="root//tools/buck2:optimized#0123456789abcdef"),
            event(end=3000.0, configuration="lash-rust-host#0123456789abcdef"),
        ]
        lines = [usage(), usage(stamp=2000, peak_bytes=2 * GIB), usage(stamp=3000, peak_bytes=3 * GIB)]
        records = list(
            joined.labelled_records(list(joined.executed_actions(events)), lines, INVENTORY)
        )
        self.assertEqual(sizes.collect_kinds(records, {KEY})[(KEY, "target")].peaks(), [GIB])
        self.assertEqual(
            sizes.collect_optimized(records)[(KEY, "target")].peaks(), [2 * GIB, 3 * GIB]
        )

    def test_one_optimized_sample_raises_the_request(self) -> None:
        # 2 GiB x 1.25 in 256 MiB steps, over a 1.5 GiB dev request.
        rows = self.table(self.measured(2 * GIB, 1))
        self.assertEqual(rows[KEY]["target"]["memory_kb"], 2621440)
        self.assertEqual(rows[KEY]["target"]["samples"], 1)

    def test_a_row_that_raises_nothing_is_dropped(self) -> None:
        self.assertEqual(self.table(self.measured(GIB // 2, 30)), {})
        current = {KEY: {"target": {"cpu_count": 1, "memory_kb": 2097152}}}
        self.assertEqual(self.table({}, dev=(1, 2097152), current=current), {})

    def test_a_lowered_dev_request_leaves_the_optimized_one_in_place(self) -> None:
        # The refresh seeds the row with the request in force before it; a few
        # small samples cannot lower it, twenty can.
        current = {KEY: {"target": {"cpu_count": 1, "memory_kb": 4194304}}}
        self.assertEqual(self.table({}, current=current)[KEY]["target"]["memory_kb"], 4194304)
        few = self.table(self.measured(GIB, 3), current=current)
        self.assertEqual(few[KEY]["target"]["memory_kb"], 4194304)
        many = self.table(self.measured(2 * GIB, 20), current=current)
        self.assertEqual(many[KEY]["target"]["memory_kb"], 2621440)

    def test_few_samples_can_still_raise_the_row_in_force(self) -> None:
        current = {KEY: {"target": {"cpu_count": 1, "memory_kb": 2097152}}}
        rows = self.table(self.measured(3 * GIB, 2), current=current)
        self.assertEqual(rows[KEY]["target"]["memory_kb"], 3932160)


class InForceTest(unittest.TestCase):
    def test_a_request_moves_by_a_cpu_or_half_a_gib_or_not_at_all(self) -> None:
        self.assertEqual(sizes.settled((1, 2097152), 1, 1835008), (1, 2097152))
        self.assertEqual(sizes.settled((1, 2097152), 1, 2359296), (1, 2097152))
        self.assertEqual(sizes.settled((1, 2097152), 1, 1572864), (1, 1572864))
        self.assertEqual(sizes.settled((1, 2097152), 2, 2097152), (2, 2097152))
        self.assertEqual(sizes.settled((1, 2097152), 1, 2359296, int(2.1 * GIB)), (1, 2359296))

    def test_a_crate_row_in_force_survives_a_small_correction(self) -> None:
        crates = {("lash-internal-core", "lash_core")}
        lines = [usage(peak_bytes=int(1.3 * GIB), requested_kb=1048576)] * 20
        current = {KEY: {"cpu_count": 1, "memory_kb": 2097152}}
        row = sizes.table(sizes.collect(lines, crates), current)[KEY]
        self.assertEqual((row["memory_kb"], row["samples"]), (2097152, 20))
        # With no row in force the default is: 1.75 GiB is 256 MiB above it.
        self.assertNotIn(KEY, sizes.table(sizes.collect(lines, crates)))
        lines = [usage(peak_bytes=int(1.7 * GIB), requested_kb=1048576)] * 20
        self.assertEqual(sizes.table(sizes.collect(lines, crates))[KEY]["memory_kb"], 2359296)
        # An unmeasured row stays as it is.
        self.assertEqual(sizes.table({}, current)[KEY], current[KEY])

    def test_a_test_run_row_in_force_survives_a_small_correction(self) -> None:
        label = "//crates/lash-core:runtime_turns__test"
        line = usage(peak_bytes=GIB // 4).replace("test=-", f"test=run:-:root{label}")
        measured = sizes.collect_test_runs([line] * 3, {label})
        self.assertEqual(sizes.test_run_table(measured)[label]["memory_kb"], 1048576)
        current = {label: {"cpu_count": 1, "memory_kb": 1310720}}
        self.assertEqual(sizes.test_run_table(measured, current)[label]["memory_kb"], 1310720)
        current = {label: {"cpu_count": 1, "memory_kb": 2097152}}
        self.assertEqual(sizes.test_run_table(measured, current)[label]["memory_kb"], 1048576)
        self.assertEqual(sizes.test_run_table({}, current)[label], current[label])


class AnonymousMemoryTest(unittest.TestCase):
    def measured(self, anon, peak=8 * GIB, count=20):
        return sizes.collect([usage(peak_bytes=peak, anon_peak_bytes=anon)] * count, {("lash-internal-core", "lash_core")})

    def test_compile_prices_anonymous_memory_with_fixed_allowance(self):
        # 1 GiB x 1.25 + 512 MiB = 1.75 GiB; seed a row to cross hysteresis.
        row = sizes.table(self.measured(GIB), {KEY: {"cpu_count": 1, "memory_kb": 4 * 1024 * 1024}})[KEY]
        self.assertEqual(row["memory_kb"], 1792 * 1024)
        self.assertEqual(row["p99_peak_bytes"], GIB)
        self.assertEqual(row["peak_bytes"], GIB)

    def test_anonymous_policy_does_not_cap_need_at_previous_request(self):
        line = usage(peak_bytes=8 * GIB, anon_peak_bytes=int(3.4 * GIB), requested_kb=4 * 1024 * 1024)
        row = sizes.compile_row(sizes.collect([line] * 20, {("lash-internal-core", "lash_core")})[KEY], (1, 4 * 1024 * 1024))
        self.assertEqual(row["memory_kb"], 4864 * 1024)

    def test_legacy_contract_keeps_peak_floor_without_inventing_anonymous_data(self):
        sample = self.measured("-", peak=3 * GIB)[KEY]
        evidence = sizes.compile_evidence({KEY: sample})[KEY]
        self.assertEqual(evidence["p99_anon_peak_bytes"], 0)
        sizes.check_compile_memory(evidence, 3840 * 1024, KEY)
        with self.assertRaises(AssertionError):
            sizes.check_compile_memory(evidence, 2 * 1024 * 1024, KEY)

    def test_compile_p99_ignores_one_anonymous_outlier(self):
        lines = [usage(peak_bytes=8 * GIB, anon_peak_bytes=GIB)] * 199
        lines += [usage(peak_bytes=8 * GIB, anon_peak_bytes=5 * GIB)]
        sample = sizes.collect(lines, {("lash-internal-core", "lash_core")})[KEY]
        row = sizes.compile_row(sample, (1, 4 * 1024 * 1024))
        self.assertEqual(row["memory_kb"], 1792 * 1024)
        sizes.check_compile_memory(sizes.compile_evidence({KEY: sample})[KEY], row["memory_kb"], KEY)

    def test_compile_contract_rejects_exact_eighty_percent(self):
        evidence = {"p99_anon_peak_bytes": 4 * GIB, "legacy_peak_bytes": 0, "oom_kills": 0, "samples": 20}
        with self.assertRaises(AssertionError):
            sizes.check_compile_memory(evidence, 5 * 1024 * 1024, KEY)
        evidence["p99_anon_peak_bytes"] -= 1
        sizes.check_compile_memory(evidence, 5 * 1024 * 1024, KEY)

    def test_killed_compiles_remain_evidence_without_successful_samples(self):
        for field in ("exit=137", "exit=247", "exit=-9", "exit=1\toom_kill=1"):
            line = usage(wall_ms=10).replace("exit=0", field)
            measured = sizes.collect([line], {("lash-internal-core", "lash_core")}, include_killed=True)
            evidence = sizes.compile_evidence(measured)[KEY]
            self.assertEqual(evidence["samples"], 0)
            self.assertEqual(evidence["oom_kills"], 1)
            with self.assertRaises(AssertionError):
                sizes.check_compile_memory(evidence, 4 * 1024 * 1024, KEY)
        self.assertEqual(sizes.collect([usage().replace("exit=0", "exit=1")], {("lash-internal-core", "lash_core")}), {})

    def test_anonymous_kinds_and_optimized_rows_use_the_same_policy(self):
        sample = self.measured(2 * GIB)[KEY]
        rows = sizes.kind_table({(KEY, "target"): sample}, {KEY: {"cpu_count": 1, "memory_kb": 4 * 1024 * 1024}})
        self.assertEqual(rows[KEY]["target"]["memory_kb"], 3 * 1024 * 1024)
        optimized = sizes.optimized_table({(KEY, "target"): sample}, lambda *_: (1, 1572864), lambda *_: (1, 1572864))
        self.assertEqual(optimized[KEY]["target"]["memory_kb"], 3 * 1024 * 1024)
        # With only one low optimized sample the previous reservation stays.
        sample = self.measured(0, count=1)[KEY]
        optimized = sizes.optimized_table({(KEY, "target"): sample}, lambda *_: (1, 1572864), lambda *_: (1, 4 * 1024 * 1024))
        self.assertEqual(optimized[KEY]["target"]["memory_kb"], 4 * 1024 * 1024)

    def test_clippy_uses_anonymous_memory_with_its_own_floor(self):
        line = usage(category="clippy", peak_bytes=8 * GIB, anon_peak_bytes=64 * 1024**2)
        samples = sizes.collect([line] * 20, {("lash-internal-core", "lash_core")}, clippy=True)
        self.assertEqual(sizes.collect([line] * 20, {("lash-internal-core", "lash_core")}), {})
        row = sizes.clippy_table(samples)[KEY]
        self.assertEqual(row["memory_kb"], 768 * 1024)
        self.assertLess(row["memory_kb"], sizes.DEFAULT_MEMORY_KB)
        legacy = usage(category="clippy", peak_bytes=64 * 1024**2)
        old = sizes.collect([legacy] * 20, {("lash-internal-core", "lash_core")}, clippy=True)
        self.assertEqual(sizes.clippy_table(old)[KEY]["memory_kb"], sizes.CLIPPY_FLOOR_KB)

    def test_clippy_kills_reject_its_separate_request(self):
        line = usage(category="clippy", wall_ms=10).replace("exit=0", "exit=137")
        optimized_ops = frozenset({sizes.parse_record(line)["op"]})
        samples = sizes.collect([line], {("lash-internal-core", "lash_core")}, optimized_ops, clippy=True, include_killed=True)
        evidence = sizes.compile_evidence(samples)[KEY]
        self.assertEqual(evidence["samples"], 0)
        self.assertEqual(evidence["oom_kills"], 1)
        with self.assertRaises(AssertionError):
            sizes.check_compile_memory(evidence, sizes.CLIPPY_FLOOR_KB, ("clippy", KEY))

    def test_test_runs_use_anonymous_peaks_and_keep_headroom(self):
        label = "//crates/lash-core:runtime_turns__test"
        line = usage(peak_bytes=3 * GIB, anon_peak_bytes=GIB).replace("test=-", f"test=run:-:{label}")
        row = sizes.test_run_table(sizes.collect_test_runs([line] * 3, {label}))[label]
        self.assertEqual(row["p99_peak_bytes"], GIB)
        self.assertEqual(row["memory_kb"], 1280 * 1024)
        self.assertEqual(sizes.HEADROOM, 0.9)


class SmallAdmissionTest(unittest.TestCase):
    def test_test_run_prices_anonymous_p99_instead_of_page_cache(self):
        label = "//crates/lash-core:runtime_turns__test"
        line = usage(peak_bytes=3 * GIB, anon_peak_bytes=100 * 1024**2).replace("test=-", f"test=run:-:{label}")
        row = sizes.test_run_table(sizes.collect_test_runs([line] * 3, {label}))[label]
        self.assertEqual(row["memory_kb"], 256 * 1024)
        self.assertEqual(row["p99_peak_bytes"], 100 * 1024**2)

    def test_unmeasured_compile_and_test_defaults_are_half_a_gib(self):
        import generate_model as model
        self.assertEqual(model.DEFAULT_MEMORY_KB, 512 * 1024)
        self.assertEqual(model.UNSIZED_ACTION_BUDGET, (1, 512 * 1024))
        self.assertEqual(model.UNMEASURED_TEST_RUN["memory_kb"], 512 * 1024)

    def test_a_killed_unmeasured_test_requires_an_explicit_floor(self):
        label = "//crates/lash-core:runtime_turns__test"
        line = usage(peak_bytes=3 * GIB, anon_peak_bytes=2 * GIB).replace("test=-", f"test=run:-:{label}").replace("exit=0", "exit=137")
        with self.assertRaisesRegex(ValueError, "explicit floor"):
            sizes.test_run_table(sizes.collect_test_runs([line], {label}))

    def test_killed_test_peak_raises_the_floor_without_becoming_a_sample(self):
        label = "//crates/lash-core:runtime_turns__test"
        line = usage(peak_bytes=3 * GIB, anon_peak_bytes=2 * GIB).replace("test=-", f"test=run:-:{label}")
        killed = line.replace("exit=0", "exit=137")
        row = sizes.test_run_table(sizes.collect_test_runs([line] * 3 + [killed], {label}))[label]
        self.assertGreaterEqual(row["memory_kb"], 3840 * 1024)
        self.assertEqual(row["samples"], 3)


class CategoryTableTest(unittest.TestCase):
    def test_a_category_counts_every_record_and_the_peak_no_row_sizes(self) -> None:
        crates = {("lash-internal-core", "lash_core")}
        lines = [
            usage(peak_bytes=3 * GIB),
            usage(peak_bytes=GIB // 8, pkg="serde", crate="serde"),
            usage(peak_bytes=GIB // 64, pkg="-", crate="-", category="deps"),
            usage(category="-"),
        ]
        self.assertEqual(
            sizes.category_table(lines, crates),
            {
                "deps": {
                    "p99_peak_bytes": GIB // 64,
                    "peak_bytes": GIB // 64,
                    "samples": 1,
                    "unsized_peak_bytes": GIB // 64,
                },
                "rustc": {
                    "p99_peak_bytes": 3 * GIB,
                    "peak_bytes": 3 * GIB,
                    "samples": 2,
                    "unsized_peak_bytes": GIB // 8,
                },
            },
        )


if __name__ == "__main__":
    unittest.main()
