#!/usr/bin/env python3
"""Refresh and inspect the per-case durations that balance libtest shards.

`tools/buck2/test-shard-weights.json` maps a sharded test's label to the
milliseconds each of its cases ran. `tools/buck2/test_shard.py` reads the row
of the test it shards and assigns the cases longest first, so the table is the
only thing besides a binary's own listing that decides which shard a case runs
on. A case the table does not name is spread round-robin, and a name the binary
no longer lists is ignored: a stale table costs balance, never coverage.

Nobody hand-edits the table. A lane that adds, renames or removes cases
changes nothing here: the round-robin fallback covers an unmeasured case and
a stale name is ignored until the next refresh replaces the row. The only
membership edit is dropping the row of a target that stops being sharded,
which the graph contracts require of every row.

The durations come from the JUnit reports the test runner writes. A case
carries a `time` only when libtest reports one, so measure with `--report-time`:

    kiln test --test_env RUSTC_BOOTSTRAP=1 \\
      --test_arg=-Z --test_arg=unstable-options --test_arg=--report-time \\
      --runs_per_test=3 --test-output-dir /tmp/shards <label>...
    python3 tools/buck2/shard_weights.py --refresh /tmp/shards/run-*/test-report.json

`--refresh` takes any number of test reports. It replaces the
row of every sharded test the reports measured with the least of each case's
times and keeps the other rows: a loaded worker only ever inflates a time, so
the least over a few runs is the quiet-pool duration. A case's time includes
whatever it waits on: measure a suite whose cases contend for one service with
`--jobs 1 --test_arg=--test-threads=1`. A feature-lane variant is folded into its
ordinary label, whose row it shares. Commit the rewritten table; it changes the
action key of every sharded test, so their next run re-executes.

`--plan LABEL COUNT` prints the cases and milliseconds the table puts on each
of COUNT shards, for choosing `shards` in `package-policy.toml`.
"""
import argparse
import json
from pathlib import Path
import re
import sys
import xml.etree.ElementTree as ET

from libtest_selection import match_name
from test_shard import VARIANT, shard_assignments

HERE = Path(__file__).resolve().parent
TABLE = HERE / "test-shard-weights.json"
SHARD = re.compile(r"__shard_\d+$")


def render(table):
    return json.dumps(table, indent=2, sort_keys=True) + "\n"


def sharded_label(label):
    """Return the ordinary label a shard's report belongs to, or None for an unsharded test."""
    label = label.removeprefix("root")
    if not SHARD.search(label):
        return None
    return VARIANT.sub("", SHARD.sub("", label))


def measured(reports):
    """Return `{label: {case: [seconds, ...]}}` over every sharded result in the reports."""
    times = {}
    for report in reports:
        results = json.loads(Path(report).read_text(encoding="utf-8"))["results"]
        for label, result in results.items():
            label = sharded_label(label)
            xml = result.get("outputs", {}).get("junit_xml")
            if label is None or not xml:
                continue
            cases = times.setdefault(label, {})
            for case in ET.parse(xml).getroot().iter("testcase"):
                # A suite-named case records the binary's exit, not a test.
                if case.get("name") != case.get("classname") and case.get("time") is not None:
                    cases.setdefault(case.get("name"), []).append(float(case.get("time")))
    return times


def refreshed(table, times):
    unmeasured = sorted(label for label, cases in times.items() if not cases)
    if unmeasured:
        raise SystemExit(
            "shard weights: no case of these tests carries a time; run them with "
            "--test_env RUSTC_BOOTSTRAP=1 --test_arg=-Z --test_arg=unstable-options --test_arg=--report-time: "
            + ", ".join(unmeasured)
        )
    table = dict(table)
    for label, cases in times.items():
        table[label] = {name: max(1, round(min(seconds) * 1000)) for name, seconds in cases.items()}
    return table


def plan(table, label, count):
    row = {match_name(name): weight for name, weight in table.get(label, {}).items()}
    assignments = shard_assignments(list(row), count, row)
    lines = []
    for shard in range(count):
        names = [name for name in sorted(row) if assignments[name] == shard]
        lines.append(f"shard {shard + 1}/{count}: {len(names)} cases, {sum(row[name] for name in names)} ms")
    return lines


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--refresh", nargs="+", metavar="REPORT", help="test reports to take case times from")
    mode.add_argument("--plan", nargs=2, metavar=("LABEL", "COUNT"), help="print the table's split of a test")
    options = parser.parse_args(argv)
    table = json.loads(TABLE.read_text(encoding="utf-8"))
    if options.plan:
        label, count = options.plan
        if label not in table:
            raise SystemExit(f"shard weights: no row for {label}")
        print("\n".join(plan(table, label, int(count))))
        return 0
    times = measured(options.refresh)
    if not times:
        raise SystemExit("shard weights: the reports name no sharded test")
    TABLE.write_text(render(refreshed(table, times)), encoding="utf-8")
    for label in sorted(times):
        print(f"{label}: {len(times[label])} cases")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
