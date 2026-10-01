#!/usr/bin/env python3
"""Self-test of scripts/latency_slope.py on synthetic ledgers of known slope."""

import contextlib
import io
import json
import random
import tempfile
import unittest
from pathlib import Path

import latency_slope


def ledger(lanes, sends, slope_of, base_of, noise=0.0, seed=4033, case="fast"):
    """`lanes` × `sends` samples: lane `l`'s ordinal `i` waits
    `base_of(l) + slope_of(l) * i` ms, plus uniform noise of ±`noise`."""

    rng = random.Random(seed)
    return [
        {"case": case, "lane": lane, "index": index,
         "accept_to_admission_ms": base_of(lane) + slope_of(lane) * index
         + (rng.uniform(-noise, noise) if noise else 0.0)}
        for lane in range(lanes) for index in range(sends)
    ]


class SlopeTests(unittest.TestCase):
    def analyse(self, samples, bins="0-99,500-599"):
        return latency_slope.analyse(samples, "fast", latency_slope.parse_bins(bins))

    def test_a_known_slope_is_recovered_exactly_without_noise(self):
        # FIG-4033's regression figure: 0.53 ms per settled turn.
        result = self.analyse(ledger(16, 625, lambda _: 0.53, lambda lane: 60.0 + lane))
        self.assertAlmostEqual(0.53, result["overall"]["ms_per_turn"], places=9)
        self.assertAlmostEqual(0.0, result["overall"]["standard_error"], places=9)
        self.assertEqual(16 * 625, result["overall"]["samples"])
        for lane in range(16):
            self.assertAlmostEqual(0.53, result["lanes"][lane]["ms_per_turn"], places=9)

    def test_a_known_slope_lies_inside_its_interval_under_noise(self):
        result = self.analyse(ledger(16, 625, lambda _: 0.53, lambda _: 60.0, noise=40.0))
        overall = result["overall"]
        self.assertLess(overall["ci95_low"], 0.53)
        self.assertGreater(overall["ci95_high"], 0.53)
        self.assertGreater(overall["ci95_low"], 0.0, "growth: the interval lies above zero")
        self.assertLess(overall["ci95_high"] - overall["ci95_low"], 0.02)

    def test_no_growth_has_an_interval_around_zero(self):
        result = self.analyse(ledger(16, 625, lambda _: 0.0, lambda _: 60.0, noise=40.0))
        overall = result["overall"]
        self.assertLess(overall["ci95_low"], 0.0)
        self.assertGreater(overall["ci95_high"], 0.0)

    def test_a_slow_lane_is_not_read_as_growth(self):
        # Flat lanes at very different levels: pooled without centring, the
        # lane order would not matter, but an uncentred fit over lanes that
        # ran different ordinal ranges would read level as slope.
        samples = ledger(1, 100, lambda _: 0.0, lambda _: 10.0)
        samples += [{**row, "lane": 1, "index": row["index"] + 500,
                     "accept_to_admission_ms": 900.0}
                    for row in ledger(1, 100, lambda _: 0.0, lambda _: 0.0)]
        result = self.analyse(samples)
        self.assertAlmostEqual(0.0, result["overall"]["ms_per_turn"], places=9)

    def test_each_lane_reports_its_own_slope(self):
        result = self.analyse(ledger(3, 200, lambda lane: 0.25 * lane, lambda _: 5.0))
        self.assertAlmostEqual(0.0, result["lanes"][0]["ms_per_turn"], places=9)
        self.assertAlmostEqual(0.25, result["lanes"][1]["ms_per_turn"], places=9)
        self.assertAlmostEqual(0.5, result["lanes"][2]["ms_per_turn"], places=9)

    def test_bin_medians_are_the_ordinal_ranges_medians(self):
        # 60 ms at ordinal 0 rising 0.5 ms per turn: FIG-4033's 61 -> 335 shape.
        result = self.analyse(ledger(4, 625, lambda _: 0.5, lambda _: 60.0))
        first, later = result["bins"]
        self.assertEqual((0, 99, 400), (first["first"], first["last"], first["samples"]))
        self.assertAlmostEqual(60.0 + 0.5 * 49.5, first["median_ms"])
        self.assertAlmostEqual(60.0 + 0.5 * 549.5, later["median_ms"])

    def test_an_empty_bin_and_an_undetermined_lane_say_so(self):
        result = self.analyse(ledger(1, 2, lambda _: 1.0, lambda _: 1.0), bins="0-0,500-599")
        self.assertIsNone(result["lanes"][0])
        self.assertIsNone(result["bins"][1]["median_ms"])
        self.assertIn("undetermined", latency_slope.render(result))
        self.assertIn("no samples", latency_slope.render(result))

    def test_other_cases_and_unmeasured_samples_are_left_out(self):
        samples = ledger(2, 50, lambda _: 1.0, lambda _: 0.0)
        samples += ledger(2, 50, lambda _: 9.0, lambda _: 0.0, case="stream")
        samples.append({"case": "fast", "lane": 0, "index": 50, "accept_to_admission_ms": None})
        result = self.analyse(samples)
        self.assertEqual(100, result["samples"])
        self.assertEqual(1, result["unmeasured"])
        self.assertAlmostEqual(1.0, result["overall"]["ms_per_turn"], places=9)

    def test_the_t_quantile_matches_the_tables(self):
        for degrees, expected in ((1, 12.706), (2, 4.303), (3, 3.182), (4, 2.776), (5, 2.571), (10, 2.228),
                                  (30, 2.042), (120, 1.980), (10_000, 1.960)):
            with self.subTest(degrees=degrees):
                self.assertAlmostEqual(expected, latency_slope.t_critical_95(degrees), delta=0.002)

    def test_the_command_prints_overall_per_lane_and_bins(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "latency-samples.json"
            path.write_text(json.dumps(ledger(2, 625, lambda _: 0.53, lambda _: 60.0)))
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(0, latency_slope.main([str(path)]))
            text = out.getvalue()
            self.assertIn("overall (lanes centred): +0.5300 ms/turn", text)
            self.assertIn("lane   1: +0.5300 ms/turn", text)
            self.assertIn("ordinals 0-99: median", text)
            self.assertIn("ordinals 500-599: median", text)
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(0, latency_slope.main([str(path), "--json", "--bins", "0-49"]))
            self.assertAlmostEqual(0.53, json.loads(out.getvalue())["overall"]["ms_per_turn"])

    def test_a_ledger_without_the_case_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "latency-samples.json"
            path.write_text("[]")
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(1, latency_slope.main([str(path)]))


if __name__ == "__main__":
    unittest.main()
