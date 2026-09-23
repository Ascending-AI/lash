#!/usr/bin/env python3
"""Turn the pool's per-action usage logs into `tools/bazel/action-sizes.json`.

Every remote action carries a `cpu_count` and a `memory_kb` request. Both are
part of the action key, so the repository defaults in `.bazelrc` are the small
action (1 CPU, 2 GiB) and never move; a compile that needs more says so per
target, from this measured table.

The input is the usage log every pool worker appends to, one tab-separated
record per action (`ACTION_USAGE_LOG`, written by the executor's action
supervisor):

    <ts>  crate=<CARGO_CRATE_NAME>  pkg=<CARGO_PKG_NAME>  kind=<...>
          tool=<argv[0]>  peak_bytes=<memory.peak>  cpu_usec=<cpu time>
          wall_ms=<wall time>  exit=<status>  requested_kb=<...>  requested_cpu=<...>

Copy the logs off the workers and run:

    python3 tools/bazel/action_sizes_from_log.py usage-*.log \\
      -o tools/bazel/action-sizes.json

What counts as a sample:

* Lash's own compiles only. The pool is shared with other repositories, so a
  record is kept only when its `(pkg, crate)` pair names a first-party target
  in this workspace's `cargo metadata`. That filter is also what keeps another
  repository's crate that happens to share a name out of this table.
* `tool=process_wrapper`: rules_rust runs every Rustc, RustcMetadata and Clippy
  action of a target through it, and nothing else in the log is a compile.
* Successful (`exit=0`) actions of at least one second of wall time. A shorter
  action cannot show how many cores it would keep busy.

Rows are keyed `<package>/<crate>`, not by kind. The `kind` field cannot key
them: rules_rust passes `--crate-type` in a params file the supervisor does not
read, so most records say `kind=-`. Every action a target owns (its compile, its
pipelined metadata compile, its Clippy pass, and for a library its unit-test
compile) shares the crate name and the `exec_properties`, so one row per
crate is also exactly what the generator can apply.

The rule, per `<package>/<crate>`:

* `cpu_count` = ceil(p95 cores - 0.2), at least 1, capped at 8, where cores is
  cpu time / wall time. A p95 within 0.2 of a whole core rounds down because CPU
  is compressible: an action that briefly wants 2.1 cores on 2 runs slightly
  slower and does not fail. Samples taken under a 1-CPU request run
  under a one-core quota and measure the cap, not the need, so they are left out
  whenever the crate also has samples at a larger request.
* `memory_kb` = the largest peak x 1.5, rounded up to 512 MiB, never below the
  2 GiB default. Memory is not compressible, so it follows the worst sample, not
  a percentile.
* At least 20 samples, or no row. Fewer samples are not enough for a p95.
* A row that asks for no more than the defaults is dropped: absence from the
  file *is* the default request.

Test *runs* are not sized here. A test run is a whole libtest binary with a
tokio runtime, not a compile, and its request lives in the generator as
`test.cpu_count` / `test.memory_kb` (see `TEST_CPU_FLOOR` there).
"""

from __future__ import annotations

import argparse
import collections
import json
import math
import pathlib
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[2]

# Must match `build --remote_default_exec_properties=...` in `.bazelrc`.
DEFAULT_MEMORY_KB = 2097152
DEFAULT_CPU_COUNT = 1

COMPILE_TOOL = "process_wrapper"
MIN_WALL_MS = 1000
MIN_SAMPLES = 20
CPU_PERCENTILE = 95
# How far above a whole core the p95 may sit and still round down.
CPU_TOLERANCE = 0.2
# The pool caps a single action's request at 8 cores.
MAX_CPU_COUNT = 8
# Peak is what the action reached on one machine on one day; the margin keeps a
# slightly larger input from being OOM-killed by the action cgroup.
MEMORY_MARGIN = 1.5
# A request is a scheduling reservation; a finer granularity only fragments the
# pool's budget.
MEMORY_GRANULARITY_KB = 512 * 1024


class Samples:
    """Every kept measurement for one `<package>/<crate>` key."""

    def __init__(self) -> None:
        # (cores, peak_bytes, requested_cpu)
        self.records: list[tuple[float, int, int]] = []

    def observe(self, cores: float, peak_bytes: int, requested_cpu: int) -> None:
        self.records.append((cores, peak_bytes, requested_cpu))

    def cpu_basis(self) -> list[float]:
        unconstrained = [r[0] for r in self.records if r[2] > DEFAULT_CPU_COUNT]
        return unconstrained or [r[0] for r in self.records]

    def peak_bytes(self) -> int:
        return max(r[1] for r in self.records)


def percentile(values: list[float], pct: int) -> float:
    """Nearest-rank percentile."""
    ordered = sorted(values)
    return ordered[max(0, math.ceil(pct / 100 * len(ordered)) - 1)]


def cpu_count_for(p95_cores: float) -> int:
    wanted = math.ceil(p95_cores - CPU_TOLERANCE)
    return min(MAX_CPU_COUNT, max(DEFAULT_CPU_COUNT, wanted))


def memory_kb_for(peak_bytes: int) -> int:
    requested = math.ceil(peak_bytes / 1024 * MEMORY_MARGIN / MEMORY_GRANULARITY_KB)
    return max(requested * MEMORY_GRANULARITY_KB, DEFAULT_MEMORY_KB)


def first_party_crates(metadata: dict) -> set[tuple[str, str]]:
    """Every `(package, crate)` pair the generator emits a target for."""
    crates = set()
    for package in metadata["packages"]:
        for target in package["targets"]:
            if "custom-build" in target["kind"]:
                continue
            crates.add((package["name"], target["name"].replace("-", "_")))
    return crates


def parse_record(line: str) -> dict[str, str] | None:
    fields = line.rstrip("\n").split("\t")
    if len(fields) < 2:
        return None
    record = {}
    for field in fields[1:]:
        key, separator, value = field.partition("=")
        if not separator:
            return None
        record[key] = value
    return record


def as_int(value: str | None) -> int | None:
    if value is None or not value.isdigit():
        return None
    return int(value)


def collect(lines, crates: set[tuple[str, str]]) -> dict[str, Samples]:
    """Keeps the Lash compile samples; every other record is skipped.

    The log is append-only from many actions at once, so a torn or unknown line
    is skipped rather than fatal: the table is a measurement, not a ledger.
    """
    measured: dict[str, Samples] = collections.defaultdict(Samples)
    for line in lines:
        record = parse_record(line)
        if record is None or record.get("tool") != COMPILE_TOOL:
            continue
        if record.get("exit") != "0":
            continue
        pair = (record.get("pkg", ""), record.get("crate", ""))
        if pair not in crates:
            continue
        wall_ms = as_int(record.get("wall_ms"))
        cpu_usec = as_int(record.get("cpu_usec"))
        peak_bytes = as_int(record.get("peak_bytes"))
        if wall_ms is None or cpu_usec is None or peak_bytes is None:
            continue
        if wall_ms < MIN_WALL_MS:
            continue
        requested_cpu = as_int(record.get("requested_cpu")) or DEFAULT_CPU_COUNT
        measured[f"{pair[0]}/{pair[1]}"].observe(
            cpu_usec / 1000 / wall_ms, peak_bytes, requested_cpu
        )
    return measured


def table(measured: dict[str, Samples]) -> dict[str, dict[str, float | int]]:
    sizes = {}
    for key, samples in measured.items():
        if len(samples.records) < MIN_SAMPLES:
            continue
        p95 = percentile(samples.cpu_basis(), CPU_PERCENTILE)
        cpu_count = cpu_count_for(p95)
        memory_kb = memory_kb_for(samples.peak_bytes())
        if cpu_count <= DEFAULT_CPU_COUNT and memory_kb <= DEFAULT_MEMORY_KB:
            continue
        sizes[key] = {
            "cpu_count": cpu_count,
            "memory_kb": memory_kb,
            "p95_cores": round(p95, 2),
            "peak_bytes": samples.peak_bytes(),
            "samples": len(samples.records),
        }
    return dict(sorted(sizes.items()))


def render(sizes: dict) -> str:
    return json.dumps(sizes, indent=2, sort_keys=True) + "\n"


def cargo_metadata() -> dict:
    return json.loads(
        subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("logs", nargs="+", type=pathlib.Path, help="pool usage logs")
    parser.add_argument(
        "-o",
        "--output",
        type=pathlib.Path,
        default=None,
        help="write the table here instead of stdout",
    )
    args = parser.parse_args()
    crates = first_party_crates(cargo_metadata())
    lines = []
    for path in args.logs:
        lines.extend(path.read_text(encoding="utf-8", errors="replace").splitlines())
    rendered = render(table(collect(lines, crates)))
    if args.output is None:
        sys.stdout.write(rendered)
    else:
        args.output.write_text(rendered, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
