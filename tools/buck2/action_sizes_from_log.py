#!/usr/bin/env python3
"""Turn the pool's per-action usage logs into `tools/buck2/action-sizes.json`.

Every remote action carries a `cpu_count` and a `memory_kb` request. Both are
part of the action key, so the defaults in `tools/buck2/platforms.bzl` are the small
action (1 CPU, 1.5 GiB) and move only deliberately; a compile that needs more
says so per target, from this measured table.

The input is the usage log every pool worker appends to, one tab-separated
record per action (`ACTION_USAGE_LOG`, written by the executor's action
supervisor):

    <ts>  crate=<CARGO_CRATE_NAME>  pkg=<CARGO_PKG_NAME>  kind=<...>
          tool=<argv[0]>  peak_bytes=<memory.peak>  cpu_usec=<cpu time>
          wall_ms=<wall time>  exit=<status>  requested_kb=<...>  requested_cpu=<...>

Copy the logs off the workers (`/workspace/kiln-executor/usage/actions.log*`)
and refresh every size file with one command:

    python3 tools/buck2/action_sizes_from_log.py --refresh usage-*.log

That rewrites `tools/buck2/action-sizes.json` and
`tools/buck2/test-run-sizes.json` and runs `tools/buck2/sync.py`, which
regenerates `tools/buck2/exec_sizes.bzl` (requests and pool budgets) from them.
Compile rows read only the Buck2 action shape (`tool=python3`), so older logs
from the Bazel era add nothing to them; `--since <unix seconds>` drops older
records when a window is wanted. `--report` prints, from the same logs, what
each Lash action category reserved against what it used.

What counts as a sample:

* Lash's own compiles only. The pool is shared with other repositories, so a
  record is kept only when its `(pkg, crate)` pair names a first-party target
  in this workspace's `cargo metadata`. That filter is also what keeps another
  repository's crate that happens to share a name out of this table.
* `tool=python3` or `tool=rustdoc`, with both Cargo identity fields naming a
  first-party target. The pinned Buck2 prelude runs Rustc, metadata and Clippy
  through `rustc_action.py` under Python; Rustdoc may run directly. Repository
  helper and build-script actions deliberately omit at least one identity, so
  this pair of conditions rejects them even though many also use Python.
* Successful (`exit=0`) actions of at least one second of wall time. A shorter
  action cannot show how many cores it would keep busy.

Rows are keyed `<package>/<crate>`, not by kind. The `kind` field cannot key
them: the compiler wrapper and argument files hide `--crate-type` from the
supervisor, so records may say `kind=-`. The compile, metadata and Clippy actions
share the Cargo identity and compile reservation. One row per crate matches the
generator's sizing key.

The rule, per `<package>/<crate>`:

* `cpu_count` = 1 while the p95 of cores (cpu time / wall time) is at most 1.6,
  otherwise ceil(p95 cores - 0.2), capped at 8. The executor's `cpu.max` for an
  action is the larger of its request and the worker's slot share (advertised
  CPUs / max_inflight: 1.67 cores on the bazelboxes, 2 on the devbox), so a
  one-CPU request already runs unthrottled up to that share and asking for two
  only reserves a core the action leaves idle. Above the share the quota is the
  request itself; a p95 within 0.2 of a whole core rounds down because CPU is
  compressible: an action that briefly wants 2.1 cores on 2 runs slightly
  slower and does not fail.
* `memory_kb` = the p99 need, never below the largest peak, rounded up to
  256 MiB, never below the 1.5 GiB default, and never below the crate's entry
  in `MINIMUM_MEMORY_KB`. A run's need is its peak x 1.25, or the request it
  ran under when the peak stayed inside that request: a run that fit proves
  its request enough, and `memory.peak` includes page cache, which fills
  whatever limit it is given. Rustc's memory is heap and not compressible, so
  no recorded compile peak may exceed the request.
* At least 20 samples, or no row -- except for a crate in
  `MINIMUM_MEMORY_KB`, whose floor keeps its row even on too few or too
  small samples. Fewer samples are not enough for a p95.
* A row that asks for no more than the defaults is dropped: absence from the
  file *is* the default request.

Test runs (`--test-runs`)
-------------------------

A test run is a whole libtest binary with a tokio runtime, not a compile, and
is sized apart from it: `--test-runs` writes `tools/buck2/test-run-sizes.json`,
which the generator passes separately to the test executor and environment. A test
action's record carries its Buck2 label in the `test` field
(`test=<run|xml>:<shard|->:<label>`, written by the supervisor since kiln#28);
the compile fields are empty for it. The rule, per label:

* Only `run` records, successful ones, of a test label this workspace
  generates (`tools/buck2/target-inventory.json`), feature lanes and
  `:test_batch` aggregates included. The XML spawn is not the test, and another
  repository's labels never match. A batch's own row is a lower bound on the
  budget the generator derives from its members (see `batch_budget` there).
* At least 3 samples, or no row -- except for a label in
  `TEST_RUN_MINIMUM_MEMORY_KB`, whose floor keeps its row even on too few
  samples. An unmeasured test keeps the request the generator gives it
  without one.
* A label with at least three Buck2 runs is measured from those alone. A
  label Buck2 has run less often also counts its Bazel-era runs
  (`tool=test-setup.sh`): the wrapper differed, the libtest binary and its
  cgroup did not.
* `memory_kb` = the p99 need as above, rounded up to 256 MiB, at least 1 GiB,
  and never below the label's entry in `TEST_RUN_MINIMUM_MEMORY_KB`. The
  largest peak is not a floor here: a test that writes files fills the page
  cache to its cgroup limit (`tool_batch_parallelism__test` peaks at exactly
  the devbox's 7.5 GiB slot share with a p99 of 0.8 GiB), so one such run
  would price the label at the box it happened to land on. A `__fv_`
  feature-variant label inherits its base label's floor.
* `cpu_count` = ceil(p95 cores - 0.2), at least 1, capped at 8, over the
  samples of at least one second. A run does not lean on the slot share as a
  compile does: a throttled compile is slower, a throttled test can pass its
  timeout. A test whose every run is shorter asks for one core: it cannot show
  more, and it holds what it has for under a second.
* Every sampled label gets a row, including one at the smallest request: the
  generator's fallback for an unmeasured run is larger than that.
"""

from __future__ import annotations

import argparse
import collections
import json
import math
import os
import pathlib
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[2]

# Must match the default request in platforms.bzl and the Rust rule macros.
DEFAULT_MEMORY_KB = 1572864
DEFAULT_CPU_COUNT = 1

COMPILE_TOOLS = {"python3", "rustdoc"}
MIN_WALL_MS = 1000
MIN_SAMPLES = 20
CPU_PERCENTILE = 95
# How far above a whole core the p95 may sit and still round down.
CPU_TOLERANCE = 0.2
# The p95 a one-CPU request covers. Every pool worker gives an action the
# larger of its request and the slot share as `cpu.max`
# (`/workspace/tools/kiln/executor/install.sh`: advertised cpu_count /
# max_inflight, 1.67 cores at the least), so one CPU is the accurate request up
# to just under that share.
SHARE_CORES = 1.6
# The pool caps a single action's request at 8 cores.
MAX_CPU_COUNT = 8
# Peak is what the action reached on one machine on one day; the margin keeps a
# slightly larger input from being OOM-killed by the action cgroup. It sits on
# each run's peak (see `memory_need`); the request is the p99 of those needs.
MEMORY_MARGIN = 1.25
MEMORY_PERCENTILE = 99
# A request is a scheduling reservation; a finer granularity only fragments the
# pool's budget.
MEMORY_GRANULARITY_KB = 256 * 1024
# Bazel's wrappers: another build system's reservations, left out of the
# waste report, and a test label's evidence only until Buck2 has its own.
BAZEL_TEST_TOOLS = {"test-setup.sh", "generate-xml.sh"}

# Explicit per-crate floors, in KiB. The table is regenerated from whatever the
# pool last measured, and a formula row can vanish on too few samples or land
# under the default. These crates are known to need more than the default, so
# a floor keeps the row; a measured need above the floor still wins. The
# values are the requests `pool-reservation-2026-09-25.md` derived from the
# 24-hour peaks of these crates' non-Rustc actions (peak x ~1.25, rounded to
# 512 MiB), which now include the Clippy/check/doc actions the row reaches
# through `//tools/buck2:pool` exec groups.
MINIMUM_MEMORY_KB = {
    "lash-internal-sqlite-store/conformance_memory": 3 * 1024 * 1024,
    "lash-internal-conformance/lash_conformance": 2560 * 1024,
    "lash-internal-remote-protocol/lash_remote_protocol": 2 * 1024 * 1024,
    "lash-internal-core-execution/lash_core_execution": 2 * 1024 * 1024,
    "lash-internal-core-execution/store_backed": 2 * 1024 * 1024,
    "lash-internal-restate-test/turn_crash_replay": 2 * 1024 * 1024,
    "lash-internal-subagents/lash_subagents": 2 * 1024 * 1024,
    "lash-internal-restate-test/suspended_turn": 2 * 1024 * 1024,
    "lash-internal-restate-test/start_gate_peek": 2 * 1024 * 1024,
    "lash-internal-core/runtime_turns": 2 * 1024 * 1024,
    "lash-internal-core-worker/lash_core_worker": 2 * 1024 * 1024,
}


class Samples:
    """Every kept measurement for one `<package>/<crate>` key."""

    def __init__(self) -> None:
        # (cores, peak_bytes, requested_cpu)
        self.records: list[tuple[float, int, int]] = []
        self.needs: list[float] = []

    def observe(
        self, cores: float, peak_bytes: int, requested_cpu: int, requested_kb: int = 0
    ) -> None:
        self.records.append((cores, peak_bytes, requested_cpu))
        self.needs.append(memory_need(peak_bytes, requested_kb))

    def cpu_basis(self) -> list[float]:
        return [r[0] for r in self.records]

    def peaks(self) -> list[int]:
        return [r[1] for r in self.records]

    def peak_bytes(self) -> int:
        return max(self.peaks())


def percentile(values: list[float], pct: int) -> float:
    """Nearest-rank percentile."""
    ordered = sorted(values)
    return ordered[max(0, math.ceil(pct / 100 * len(ordered)) - 1)]


def cpu_count_for(p95_cores: float, within_share: bool = True) -> int:
    """The compile request; a test run passes `within_share=False`."""
    if within_share and p95_cores <= SHARE_CORES:
        return DEFAULT_CPU_COUNT
    wanted = math.ceil(p95_cores - CPU_TOLERANCE)
    return min(MAX_CPU_COUNT, max(DEFAULT_CPU_COUNT, wanted))


def memory_need(peak_bytes: int, requested_kb: int) -> float:
    """What one successful run shows it needs, in bytes: its peak with margin.

    A run that stayed inside its own request proves that request enough, so
    its need stops there. `memory.peak` counts the page cache the action
    filled, which grows to whatever limit the cgroup has; without this bound a
    run that sat at its limit asks for a quarter more at every refresh.
    """
    need = peak_bytes * MEMORY_MARGIN
    if peak_bytes <= requested_kb * 1024:
        need = min(need, requested_kb * 1024)
    return need


def memory_kb_for(
    needs: list[float], floor_kb: int = DEFAULT_MEMORY_KB, floor_bytes: int = 0
) -> int:
    """The p99 need, never below `floor_bytes`, rounded up to the granularity."""
    wanted_bytes = max(percentile(needs, MEMORY_PERCENTILE), floor_bytes)
    requested = math.ceil(wanted_bytes / 1024 / MEMORY_GRANULARITY_KB)
    return max(requested * MEMORY_GRANULARITY_KB, floor_kb)


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
        if record is None or record.get("tool") not in COMPILE_TOOLS:
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
            cpu_usec / 1000 / wall_ms,
            peak_bytes,
            requested_cpu,
            as_int(record.get("requested_kb")) or 0,
        )
    return measured


TEST_MIN_SAMPLES = 3
TEST_MEMORY_FLOOR_KB = 1024 * 1024
TEST_RUN_KINDS = ("unit-test", "bin-unit-test", "test")

# Explicit per-label floors, in KiB. The `peak_bytes` the log measures is the
# action cgroup's `memory.peak`, which charges only the pages that action
# faulted: on a warm box an earlier action already holds the test binary and
# its runfiles, so the logged peak can sit far below the footprint a run
# carries when it faults those pages itself. `//crates/lash:lash__unit_test`
# logged 181 MiB across warm runs but peaked at 2.0 GiB when it faulted its
# own pages -- inside the ~2.08 GiB share the dense pool worker gives an
# action, where the kernel then OOM-killed it mid-suite (FIG-3882). A floor
# keeps such a label at the request its own-faulted footprint needs (peak x
# 1.5) even while the measured samples stay warm. A `__fv_` feature variant
# is the same binary under another resolution and inherits its base label's
# floor, as it does a measured row in `test_run_request`.
# `//crates/lash-sim:lash-sim__unit_test` logged 220 MiB, but a shard running
# four whole-deployment simulations at once peaks at 2.3 GiB when it faults its
# own pages, and its heaviest shards were OOM-killed at the logged request.
TEST_RUN_MINIMUM_MEMORY_KB = {
    "//crates/lash:lash__unit_test": 3 * 1024 * 1024,
    "//crates/lash-sim:lash-sim__unit_test": 3584 * 1024,
}


def test_labels(inventory: dict) -> set[str]:
    """Every test label the generator emits: tests, feature lanes and batches."""
    units = [
        target for package in inventory["packages"] for target in package["targets"]
    ] + inventory["feature_lane_units"]
    return {
        unit["label"]
        for unit in units
        if unit.get("label") and unit["kind"] in TEST_RUN_KINDS
    } | {
        package["test_batch"]["label"]
        for package in inventory["packages"]
        if package["test_batch"]["label"]
    }


class TestRuns:
    """Every kept run of one test label.

    A run of under a second still counts as a sample and still has a peak, but
    its cores are process startup, not a need, so only longer runs are CPU
    evidence.
    """

    def __init__(self) -> None:
        self.peaks: list[int] = []
        self.needs: list[float] = []
        self.timed = Samples()

    @property
    def count(self) -> int:
        return len(self.peaks)

    @property
    def peak_bytes(self) -> int:
        return max(self.peaks, default=0)

    def absorb(self, other: "TestRuns") -> None:
        self.peaks += other.peaks
        self.needs += other.needs
        self.timed.records += other.timed.records
        self.timed.needs += other.timed.needs

    def observe(
        self,
        cpu_usec: int,
        wall_ms: int,
        peak_bytes: int,
        requested_cpu: int,
        requested_kb: int = 0,
    ) -> None:
        self.peaks.append(peak_bytes)
        self.needs.append(memory_need(peak_bytes, requested_kb))
        if wall_ms >= MIN_WALL_MS:
            self.timed.observe(cpu_usec / 1000 / wall_ms, peak_bytes, requested_cpu)


def collect_test_runs(lines, labels: set[str]) -> dict[str, TestRuns]:
    """Keeps the successful runs of this workspace's tests, keyed by label.

    A label with at least TEST_MIN_SAMPLES Buck2 runs is measured from those
    alone; only a label Buck2 has not run that often yet falls back to its
    Bazel-era runs as well.
    """
    measured: dict[str, TestRuns] = collections.defaultdict(TestRuns)
    legacy: dict[str, TestRuns] = collections.defaultdict(TestRuns)
    for line in lines:
        record = parse_record(line)
        if record is None or record.get("exit") != "0":
            continue
        role, _, rest = record.get("test", "-").partition(":")
        _shard, _, label = rest.partition(":")
        if label.startswith("root//"):
            label = label.removeprefix("root")
        if role != "run" or label not in labels:
            continue
        wall_ms = as_int(record.get("wall_ms"))
        cpu_usec = as_int(record.get("cpu_usec"))
        peak_bytes = as_int(record.get("peak_bytes"))
        if wall_ms is None or cpu_usec is None or peak_bytes is None:
            continue
        requested_cpu = as_int(record.get("requested_cpu")) or DEFAULT_CPU_COUNT
        era = legacy if record.get("tool") in BAZEL_TEST_TOOLS else measured
        era[label].observe(
            cpu_usec, wall_ms, peak_bytes, requested_cpu, as_int(record.get("requested_kb")) or 0
        )
    for label, runs in legacy.items():
        if measured[label].count < TEST_MIN_SAMPLES:
            measured[label].absorb(runs)
    return {label: runs for label, runs in measured.items() if runs.count}


def test_run_table(measured: dict[str, TestRuns]) -> dict[str, dict[str, float | int]]:
    sizes = {}
    for label in sorted(set(measured) | set(TEST_RUN_MINIMUM_MEMORY_KB)):
        runs = measured.get(label, TestRuns())
        minimum = TEST_RUN_MINIMUM_MEMORY_KB.get(
            label, TEST_RUN_MINIMUM_MEMORY_KB.get(label.split("__fv_", 1)[0])
        )
        if runs.count < TEST_MIN_SAMPLES and minimum is None:
            continue
        p95 = (
            percentile(runs.timed.cpu_basis(), CPU_PERCENTILE)
            if runs.timed.records
            else 0.0
        )
        sizes[label] = {
            "cpu_count": cpu_count_for(p95, within_share=False),
            "memory_kb": max(
                memory_kb_for(runs.needs, floor_kb=TEST_MEMORY_FLOOR_KB)
                if runs.peaks
                else TEST_MEMORY_FLOOR_KB,
                minimum or 0,
            ),
            "p95_cores": round(p95, 2),
            "p99_peak_bytes": percentile(runs.peaks, MEMORY_PERCENTILE) if runs.peaks else 0,
            "peak_bytes": runs.peak_bytes,
            "samples": runs.count,
        }
    return dict(sorted(sizes.items()))


def table(measured: dict[str, Samples]) -> dict[str, dict[str, float | int]]:
    sizes = {}
    for key in sorted(set(measured) | set(MINIMUM_MEMORY_KB)):
        samples = measured.get(key, Samples())
        minimum = MINIMUM_MEMORY_KB.get(key)
        if len(samples.records) < MIN_SAMPLES and minimum is None:
            continue
        p95 = percentile(samples.cpu_basis(), CPU_PERCENTILE) if samples.records else 0.0
        cpu_count = cpu_count_for(p95)
        memory_kb = max(
            memory_kb_for(samples.needs, floor_bytes=samples.peak_bytes())
            if samples.records
            else 0,
            minimum or 0,
        )
        if cpu_count <= DEFAULT_CPU_COUNT and memory_kb <= DEFAULT_MEMORY_KB:
            continue
        sizes[key] = {
            "cpu_count": cpu_count,
            "memory_kb": memory_kb,
            "p95_cores": round(p95, 2),
            "p99_peak_bytes": (
                percentile(samples.peaks(), MEMORY_PERCENTILE) if samples.records else 0
            ),
            "peak_bytes": samples.peak_bytes() if samples.records else 0,
            "samples": len(samples.records),
        }
    return dict(sorted(sizes.items()))


def render(sizes: dict) -> str:
    return json.dumps(sizes, indent=2, sort_keys=True) + "\n"


def cargo_metadata() -> dict:
    cargo = os.environ.get("KILN_REAL_CARGO")
    if not cargo:
        raise SystemExit("source ./env.sh first (KILN_REAL_CARGO is unset)")
    return json.loads(
        subprocess.run(
            [cargo, "metadata", "--no-deps", "--format-version", "1", "--locked"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    )


def category_of(record: dict[str, str], crates: set[tuple[str, str]], labels: set[str]) -> str | None:
    """The Lash action category a record belongs to, or None for another repository's.

    The supervisor sees argv[0], the Cargo identity and the test label, not
    the Buck2 category, so these are the groups the log can tell apart. A
    target's Rustc, metadata, Clippy and Rustdoc actions share its identity
    and, by Buck2's one execution platform per target, its request.
    """
    tool = record.get("tool")
    if tool in BAZEL_TEST_TOOLS or tool == "process_wrapper":
        return None
    role, _, rest = record.get("test", "-").partition(":")
    if role == "run":
        label = rest.partition(":")[2].removeprefix("root")
        if label not in labels:
            return None
        return "test batch" if label.endswith(":test_batch") else "test run"
    if tool not in COMPILE_TOOLS:
        return None
    pair = (record.get("pkg", "-"), record.get("crate", "-"))
    if pair in crates:
        return "rustc, first-party (rustc, metadata, clippy, rustdoc)"
    if pair[1] == "build_script_build":
        return "rustc, build script"
    if "-" in pair:
        return "helper (deps, http_archive, schema, failure_filter)"
    return "rustc, third-party"


def waste_report(lines, crates: set[tuple[str, str]], labels: set[str]) -> str:
    """Per category: what it reserved against what it used.

    `excess` is reserved minus used CPU-seconds, the weight count x duration x
    excess; `memory x` is reserved over peak byte-seconds. Failed actions count
    here (they held their reservation too) but never in a size table.
    """
    totals: dict[str, list[float]] = collections.defaultdict(lambda: [0.0] * 8)
    for line in lines:
        record = parse_record(line)
        if record is None:
            continue
        category = category_of(record, crates, labels)
        fields = [
            as_int(record.get(name))
            for name in ("wall_ms", "cpu_usec", "peak_bytes", "requested_cpu", "requested_kb")
        ]
        if category is None or None in fields:
            continue
        wall_ms, cpu_usec, peak_bytes, requested_cpu, requested_kb = fields
        row = totals[category]
        row[0] += 1
        row[1] += wall_ms / 1000
        row[2] += cpu_usec / 1e6
        row[3] += requested_cpu * wall_ms / 1000
        row[4] += requested_kb * 1024 * wall_ms
        row[5] += peak_bytes * wall_ms
        row[6] += peak_bytes > requested_kb * 1024
        row[7] += record.get("exit") != "0"
    out = [
        "| category | actions | wall s | used CPU s | reserved CPU s | excess | CPU x | memory x | peak > request | failed |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    total = [0.0] * 8
    for category, row in sorted(totals.items(), key=lambda item: item[1][2] - item[1][3]):
        total = [a + b for a, b in zip(total, row)]
        out.append(report_row(category, row))
    out.append(report_row("all", total))
    return "\n".join(out) + "\n"


def report_row(name: str, row: list[float]) -> str:
    return (
        f"| {name} | {row[0]:.0f} | {row[1]:.0f} | {row[2]:.0f} | {row[3]:.0f} "
        f"| {row[3] - row[2]:.0f} | {row[3] / row[2] if row[2] else 0:.2f} "
        f"| {row[4] / row[5] if row[5] else 0:.2f} | {row[6]:.0f} | {row[7]:.0f} |"
    )


def read_lines(paths: list[pathlib.Path], since: int) -> list[str]:
    lines = []
    for path in paths:
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            stamp = line.partition("\t")[0]
            if since and (not stamp.isdigit() or int(stamp) < since):
                continue
            lines.append(line)
    return lines


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
    parser.add_argument(
        "--test-runs",
        action="store_true",
        help="size test runs per label (tools/buck2/test-run-sizes.json) instead of compiles",
    )
    parser.add_argument(
        "--refresh",
        action="store_true",
        help="rewrite both size files in place and regenerate exec_sizes.bzl",
    )
    parser.add_argument(
        "--report",
        action="store_true",
        help="print reserved against used per action category instead of a table",
    )
    parser.add_argument(
        "--since",
        type=int,
        default=0,
        metavar="UNIX_SECONDS",
        help="ignore records older than this",
    )
    args = parser.parse_args()
    lines = read_lines(args.logs, args.since)
    inventory = json.loads(
        (ROOT / "tools/buck2/target-inventory.json").read_text(encoding="utf-8")
    )
    if args.refresh or args.report:
        crates = first_party_crates(cargo_metadata())
        if args.report:
            sys.stdout.write(waste_report(lines, crates, test_labels(inventory)))
            return 0
        (ROOT / "tools/buck2/action-sizes.json").write_text(
            render(table(collect(lines, crates))), encoding="utf-8"
        )
        (ROOT / "tools/buck2/test-run-sizes.json").write_text(
            render(test_run_table(collect_test_runs(lines, test_labels(inventory)))),
            encoding="utf-8",
        )
        return subprocess.run(
            [sys.executable, str(ROOT / "tools/buck2/sync.py")], cwd=ROOT
        ).returncode
    if args.test_runs:
        rendered = render(test_run_table(collect_test_runs(lines, test_labels(inventory))))
    else:
        rendered = render(table(collect(lines, first_party_crates(cargo_metadata()))))
    if args.output is None:
        sys.stdout.write(rendered)
    else:
        args.output.write_text(rendered, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
