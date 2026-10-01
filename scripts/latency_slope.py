#!/usr/bin/env python3
"""Accept-to-admission slope and ordinal-bin medians of a latency ledger.

    scripts/latency_slope.py latency-samples.json [--case fast] [--bins 0-99,500-599]

Reads the raw per-sample ledger `lash-perf latency --samples-out` writes: one
object per sample, with its `case`, its `lane`, its `index` (the sample's
ordinal among its lane's sends, so the count of turns the lane's session had
settled before it) and `accept_to_admission_ms`.

For one case it prints:

* the least-squares slope of accept-to-admission against the ordinal, in
  ms per turn, with a 95 % confidence interval: over all lanes together
  (each lane centred on its own means, so a lane that is slow throughout
  does not read as growth), and for each lane alone;
* the median accept-to-admission of each ordinal bin, 0-99 and 500-599
  unless `--bins` names others.

FIG-4033's criterion is a slope of about 0 ms per turn: no growth with the
session's settled history. A slope whose interval lies above zero is growth.
`--json` prints the same numbers as one JSON object.
"""

from __future__ import annotations

import argparse
import json
import math
import statistics
import sys
from pathlib import Path

DEFAULT_BINS = "0-99,500-599"


def t_critical_95(degrees: int) -> float:
    """The two-sided 95 % Student-t critical value, by the Cornish-Fisher
    expansion about the normal quantile (within 0.002 of the tables from 5
    degrees of freedom; the tabulated values below that)."""

    exact = {1: 12.706, 2: 4.303, 3: 3.182, 4: 2.776}
    if degrees in exact:
        return exact[degrees]
    z = 1.959963984540054
    g1 = (z**3 + z) / 4
    g2 = (5 * z**5 + 16 * z**3 + 3 * z) / 96
    g3 = (3 * z**7 + 19 * z**5 + 17 * z**3 - 15 * z) / 384
    g4 = (79 * z**9 + 776 * z**7 + 1482 * z**5 - 1920 * z**3 - 945 * z) / 92160
    return z + g1 / degrees + g2 / degrees**2 + g3 / degrees**3 + g4 / degrees**4


def slope(points: list[tuple[float, float]], groups: int = 1) -> dict | None:
    """The least-squares slope of y on x over `points`, already centred when
    `groups` > 1 (one intercept per group is spent). None when the slope is
    undetermined: fewer than three points, or one ordinal only."""

    n = len(points)
    degrees = n - groups - 1
    if degrees < 1:
        return None
    mean_x = sum(x for x, _ in points) / n
    mean_y = sum(y for _, y in points) / n
    sxx = sum((x - mean_x) ** 2 for x, _ in points)
    if sxx == 0:
        return None
    sxy = sum((x - mean_x) * (y - mean_y) for x, y in points)
    estimate = sxy / sxx
    residual = sum(((y - mean_y) - estimate * (x - mean_x)) ** 2 for x, y in points)
    standard_error = math.sqrt(residual / degrees / sxx)
    half_width = t_critical_95(degrees) * standard_error
    return {
        "ms_per_turn": estimate,
        "ci95_low": estimate - half_width,
        "ci95_high": estimate + half_width,
        "standard_error": standard_error,
        "samples": n,
    }


def pooled_slope(lanes: dict[int, list[tuple[float, float]]]) -> dict | None:
    """One slope over every lane, each lane centred on its own means."""

    centred: list[tuple[float, float]] = []
    for points in lanes.values():
        if not points:
            continue
        mean_x = sum(x for x, _ in points) / len(points)
        mean_y = sum(y for _, y in points) / len(points)
        centred.extend((x - mean_x, y - mean_y) for x, y in points)
    return slope(centred, groups=max(len(lanes), 1))


def parse_bins(text: str) -> list[tuple[int, int]]:
    bins = []
    for part in text.split(","):
        low, _, high = part.strip().partition("-")
        if not high or int(low) > int(high):
            raise ValueError(f"a bin is `<first>-<last>` ordinals, got `{part}`")
        bins.append((int(low), int(high)))
    return bins


def analyse(samples: list[dict], case: str, bins: list[tuple[int, int]]) -> dict:
    measured = [row for row in samples
                if row.get("case") == case and row.get("accept_to_admission_ms") is not None]
    lanes: dict[int, list[tuple[float, float]]] = {}
    for row in measured:
        lanes.setdefault(row["lane"], []).append(
            (float(row["index"]), float(row["accept_to_admission_ms"])))
    medians = []
    for low, high in bins:
        values = [row["accept_to_admission_ms"] for row in measured if low <= row["index"] <= high]
        medians.append({"first": low, "last": high, "samples": len(values),
                        "median_ms": statistics.median(values) if values else None})
    return {
        "case": case,
        "samples": len(measured),
        "unmeasured": sum(1 for row in samples
                          if row.get("case") == case and row.get("accept_to_admission_ms") is None),
        "overall": pooled_slope(lanes),
        "lanes": {lane: slope(points) for lane, points in sorted(lanes.items())},
        "bins": medians,
    }


def render_slope(found: dict | None) -> str:
    if found is None:
        return "undetermined (too few samples or ordinals)"
    return (f"{found['ms_per_turn']:+.4f} ms/turn, 95% CI [{found['ci95_low']:+.4f}, "
            f"{found['ci95_high']:+.4f}], n={found['samples']}")


def render(result: dict) -> str:
    lines = [f"accept-to-admission slope, case {result['case']}: {result['samples']} samples"
             f" ({result['unmeasured']} without an admission instant)",
             f"  overall (lanes centred): {render_slope(result['overall'])}"]
    for lane, found in result["lanes"].items():
        lines.append(f"  lane {lane:>3}: {render_slope(found)}")
    for row in result["bins"]:
        median = "no samples" if row["median_ms"] is None else f"{row['median_ms']:.3f} ms"
        lines.append(f"  ordinals {row['first']}-{row['last']}: median {median}, n={row['samples']}")
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("samples", type=Path, help="the ledger `--samples-out` wrote")
    parser.add_argument("--case", default="fast")
    parser.add_argument("--bins", default=DEFAULT_BINS, help="ordinal bins, `<first>-<last>,...`")
    parser.add_argument("--json", action="store_true", help="print one JSON object")
    args = parser.parse_args(argv)
    samples = json.loads(args.samples.read_text(encoding="utf-8"))
    result = analyse(samples, args.case, parse_bins(args.bins))
    if result["samples"] == 0:
        print(f"latency slope: the ledger holds no measured `{args.case}` sample", file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2) if args.json else render(result))
    return 0


if __name__ == "__main__":
    sys.exit(main())
