#!/usr/bin/env python3
"""Turn measured rustc invocations into `tools/bazel/action-sizes.json`.

Every remote action carries a `memory_kb` and a `cpu_count` request. Those two
numbers are part of the action key, so the repository defaults in `.bazelrc`
are deliberately small and fixed; a target that needs more says so per target,
from a measured table rather than by moving the defaults.

This script builds that table from a usage log. Each line is one rustc
invocation:

    crate=<--crate-name> kind=<lib|bin|test|build-script> <rss_kb> <user> <sys> <wall>

where `rss_kb` is maximum resident set size in KiB and the three times are
seconds. The seeding log is produced locally by
`tools/bazel/measure_rustc_usage.sh`, a `RUSTC_WRAPPER` that runs the real rustc
under `/usr/bin/time`:

    LASH_RUSTC_USAGE_LOG=/tmp/usage.log \
      RUSTC_WRAPPER=$PWD/tools/bazel/measure_rustc_usage.sh \
      cargo build --workspace --all-targets --locked
    python3 tools/bazel/action_sizes_from_log.py /tmp/usage.log \
      -o tools/bazel/action-sizes.json

The pool's own per-box usage log (kiln
`executor/tools/action-sizes`) emits the same JSON, so replacing the seed with
pool measurements is a file replace and not a format change.

The rule, per `<crate>/<kind>`:

* `memory_kb` = peak RSS x 1.5, rounded up to a multiple of 512 MiB, never
  below 1 GiB, and never below the repository default (a table entry may only
  raise a request; lowering one would OOM-kill the action it describes).
* `cpu_count` = ceil(cpu-seconds / wall-seconds), at least 1, capped at 8.
* Both are taken over the loudest sample of that key, not the mean: the
  request has to hold the worst invocation the key ever produced.

Entries that ask for no more than the defaults are dropped: absence from the
file *is* the default request.
"""

from __future__ import annotations

import argparse
import collections
import json
import math
import pathlib
import sys


# Must match `build:shared --remote_default_exec_properties=...` in `.bazelrc`.
DEFAULT_MEMORY_KB = 2097152
DEFAULT_CPU_COUNT = 1

# Peak is what the action reached on one machine on one day; the margin is what
# keeps a slightly larger input from being OOM-killed by the action cgroup.
MEMORY_MARGIN = 1.5
# A request is a scheduling reservation, so a finer granularity would only
# fragment the pool's budget.
MEMORY_GRANULARITY_KB = 512 * 1024
MEMORY_FLOOR_KB = 1024 * 1024
# The pool caps a single action's request at 8 cores (kiln
# `executor/tools/action-sizes`, CPU_CAP = 8, under a per-box ceiling of half a
# box). The cap is repeated here so this seeded table and the one the pool
# regenerates from its own per-box logs agree row for row instead of the seed
# quietly asking for less.
MAX_CPU_COUNT = 8

KINDS = ("lib", "bin", "test", "build-script")

# Every package's `build.rs` compiles under this one rustc crate name, so a row
# built from it would be one merged number standing for forty-five unrelated
# compiles. It is also unusable: `cargo_build_script` forwards its keyword
# arguments to the rule that RUNS a build script, not to the `rust_binary` that
# compiles it, so a measured compile has no target to attach to. A
# `<package>/build-script` row measured on the run action is the one this
# repository's generator consumes; this local wrapper cannot produce one.
UNKEYABLE_CRATES = ("build_script_build",)


class Sample:
    """The loudest measurement seen for one `<crate>/<kind>` key."""

    def __init__(self) -> None:
        self.peak_rss_kb = 0
        self.cpu_count = DEFAULT_CPU_COUNT
        self.samples = 0

    def observe(self, rss_kb: int, user: float, sys_: float, wall: float) -> None:
        self.samples += 1
        self.peak_rss_kb = max(self.peak_rss_kb, rss_kb)
        self.cpu_count = max(self.cpu_count, cpu_count_for(user, sys_, wall))


def cpu_count_for(user: float, sys_: float, wall: float) -> int:
    """Cores the invocation actually kept busy, rounded up."""
    if wall <= 0:
        # Below the clock's resolution: nothing that short parallelises.
        return DEFAULT_CPU_COUNT
    return min(
        MAX_CPU_COUNT, max(DEFAULT_CPU_COUNT, math.ceil((user + sys_) / wall))
    )


def memory_kb_for(peak_rss_kb: int) -> int:
    requested = math.ceil(peak_rss_kb * MEMORY_MARGIN / MEMORY_GRANULARITY_KB)
    requested *= MEMORY_GRANULARITY_KB
    return max(requested, MEMORY_FLOOR_KB, DEFAULT_MEMORY_KB)


def parse_log(lines: list[str]) -> dict[str, Sample]:
    measured: dict[str, Sample] = collections.defaultdict(Sample)
    for number, line in enumerate(lines, start=1):
        line = line.strip()
        if not line:
            continue
        fields = line.split()
        if len(fields) != 6:
            raise ValueError(f"line {number}: expected 6 fields, got {len(fields)}")
        crate_field, kind_field, rss_field, user, sys_, wall = fields
        if not crate_field.startswith("crate=") or not kind_field.startswith("kind="):
            raise ValueError(f"line {number}: expected `crate=... kind=...`")
        crate = crate_field[len("crate=") :]
        kind = kind_field[len("kind=") :]
        if not crate:
            raise ValueError(f"line {number}: empty crate name")
        if kind not in KINDS:
            raise ValueError(f"line {number}: unknown kind {kind!r}")
        measured[f"{crate}/{kind}"].observe(
            int(rss_field), float(user), float(sys_), float(wall)
        )
    return measured


def table(measured: dict[str, Sample]) -> dict[str, dict[str, int]]:
    sizes = {}
    for key, sample in measured.items():
        if key.split("/", 1)[0] in UNKEYABLE_CRATES:
            continue
        memory_kb = memory_kb_for(sample.peak_rss_kb)
        if memory_kb <= DEFAULT_MEMORY_KB and sample.cpu_count <= DEFAULT_CPU_COUNT:
            continue
        sizes[key] = {
            "cpu_count": sample.cpu_count,
            "memory_kb": memory_kb,
            "peak_bytes": sample.peak_rss_kb * 1024,
            "samples": sample.samples,
        }
    return dict(sorted(sizes.items()))


def render(sizes: dict[str, dict[str, int]]) -> str:
    return json.dumps(sizes, indent=2, sort_keys=True) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=pathlib.Path, help="measured rustc usage log")
    parser.add_argument(
        "-o",
        "--output",
        type=pathlib.Path,
        default=None,
        help="write the table here instead of stdout",
    )
    args = parser.parse_args()
    rendered = render(table(parse_log(args.log.read_text(encoding="utf-8").splitlines())))
    if args.output is None:
        sys.stdout.write(rendered)
    else:
        args.output.write_text(rendered, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
