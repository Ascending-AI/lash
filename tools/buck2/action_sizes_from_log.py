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
          test=<...>  op=<...>  category=<Buck2 category>  anon_peak_bytes=<...>

Copy the logs off the workers (`/workspace/kiln-executor/usage/actions.log*`)
and refresh every size file with one command:

    python3 tools/buck2/action_sizes_from_log.py --refresh \\
      --events buck-out/kiln/log/*_events.pb.zst -- usage-*.log

That rewrites `tools/buck2/action-sizes.json`,
`tools/buck2/target-kind-sizes.json`, `tools/buck2/optimized-sizes.json`,
`tools/buck2/clippy-sizes.json`, `tools/buck2/category-sizes.json` and
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
supervisor, so records may say `kind=-`. The compile and metadata actions
share the Cargo identity and compile reservation. One row per crate matches the
generator's sizing key. Clippy runs on a target of its own and is sized apart
(see "Clippy" below), so a record of category `clippy` is never a compile
sample.

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
  in `MINIMUM_MEMORY_KB`. A run's peak is the larger of `peak_bytes` and
  `anon_peak_bytes`; its need is that peak x 1.25, or the request it ran under
  when the peak stayed under 90% of that request: a run that fit with room
  proves its request enough, and `memory.peak` includes page cache, which
  fills whatever limit it is given. Rustc's memory is heap and not compressible, so no recorded
  compile peak may exceed the request.
* At least 20 samples, or the row in force stays -- a crate in
  `MINIMUM_MEMORY_KB` keeps its floor even on too few or too small samples.
  Fewer samples are not enough for a p95.
* A request is a platform property and so part of the action key: resizing a
  row re-executes every action of its targets on a cold cache. A refresh
  therefore keeps the row in force unless the request moves by a whole CPU or
  by at least 512 MiB, or a recorded peak exceeds it.
* A row that asks for no more than the defaults is dropped: absence from the
  file *is* the default request.

Target kinds (`--events`)
-------------------------

Buck2 resolves one execution platform per target, so a request is per target,
not per action: a target's `rustc`, `clippy`, `rustdoc_json` and `deps`
actions share it. But a library (or binary) and the unit-test binary built
from the same crate root are two targets with one Cargo identity, and they
need very different requests: a library runs metadata and rlib actions, its
test binary a link that peaks several times higher. The usage log cannot
tell them apart, so `--events` names Buck2 event logs to join against it
(`action_categories_from_events.py` documents the join), and
`tools/buck2/target-kind-sizes.json` carries what the join measured, keyed
`<package>/<crate>` then `target` (the library or binary) or `test` (its
unit-test binary):

* Only an identity that names both kinds of target gets rows; an integration
  test's crate names one target, and its crate row is already its own.
* A kind is sized by the compile rule above from its matched records alone,
  at least 20 of them. A kind with fewer keeps the crate row, which is
  measured from every record of the identity and so covers whichever target
  is heavier. A kind row may equal the default request: it is the statement
  that the library fits there although its crate row is larger.
* Without `--events` the kind rows are left as they are, less any whose crate
  is gone.

Optimized compiles
------------------

`--config=optimized` and the host configuration (proc macros, build-script
dependencies) compile with `-Copt-level=3`, and LLVM then holds more than a
dev compile of the same crate. The event logs name each action's
configuration, so `tools/buck2/optimized-sizes.json` carries, keyed like the
kind table, what an optimized compile asks for where that is more than the
dev request; the generated rules select it on the profile constraint, so a
dev build's requests do not depend on it. The rule is `optimized_table`'s: a row only raises, one optimized sample is
evidence, and a dev request that a refresh lowers leaves the optimized
request where it was until 20 optimized samples say otherwise.

Clippy
------

Every generated Rust target has a Clippy twin, `<label>__clippy`: the same
rule and attributes on an execution platform of its own, because Buck2 gives a
target one platform and Clippy needs a fraction of a compile's memory.
`tools/buck2/clippy-sizes.json` carries its request, keyed `<package>/<crate>`
from the records of category `clippy` (a library's and its unit-test binary's
Clippy share the identity, so the row covers the heavier). The rule is the
compile rule above with a 512 MiB floor (`CLIPPY_FLOOR_KB`) in place of the
1.5 GiB default. A crate without a row keeps its compile request for Clippy.
A refresh adds a row for every crate with at least 20 samples, and a row in
force moves only as a compile row does.

Categories
----------

`tools/buck2/category-sizes.json` records, per Buck2 category, how many
actions the logs hold and the largest peak of those no row sizes (helpers,
build scripts, third-party compiles: whatever has no first-party Cargo
identity). The graph contracts hold that peak to the smallest request such an
action can run under.

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
  and never below the label's entry in `TEST_RUN_MINIMUM_MEMORY_KB`. A
  measured row in force stays unless the request moves by a whole CPU or by
  at least 512 MiB, or the p99 peak reaches 90% of it. The
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
# The least a measured Clippy row asks for: Clippy's median peak is under
# 100 MiB, and a smaller floor would only register more execution platforms.
# Must match `CLIPPY_FLOOR_KB` in `tools/buck2/generate_model.py`.
CLIPPY_FLOOR_KB = 512 * 1024

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
# The share of its request a run may fill and still count as fitting it. A
# test-run row holds its p99 peak under this share; the graph contracts check
# it.
HEADROOM = 0.9
MEMORY_PERCENTILE = 99
# A request is a scheduling reservation; a finer granularity only fragments the
# pool's budget.
MEMORY_GRANULARITY_KB = 256 * 1024
# A request is part of the action key. A row moves only when the reservation
# changes by this much or by a whole CPU; smaller corrections are not worth a
# cold rebuild of the row's targets.
MEMORY_HYSTERESIS_KB = 512 * 1024
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

    A run that stayed well inside its own request proves that request enough,
    so its need stops there: `memory.peak` counts the page cache the action
    filled, and without this bound every refresh would add a quarter. A run
    that came within `HEADROOM` of its request proves nothing of the kind. The
    cgroup kills at the request, so the runs that needed a little more are the
    ones that died and left no sample: `//crates/lash:lash__unit_test` sat at
    2.9 GiB of a 3 GiB request for days while one run in eight was OOM-killed.
    """
    need = peak_bytes * MEMORY_MARGIN
    if peak_bytes < HEADROOM * requested_kb * 1024:
        need = min(need, requested_kb * 1024)
    return need


def memory_kb_for(
    needs: list[float], floor_kb: int = DEFAULT_MEMORY_KB, floor_bytes: int = 0
) -> int:
    """The p99 need, never below `floor_bytes`, rounded up to the granularity."""
    wanted_bytes = max(percentile(needs, MEMORY_PERCENTILE), floor_bytes)
    requested = math.ceil(wanted_bytes / 1024 / MEMORY_GRANULARITY_KB)
    return max(requested * MEMORY_GRANULARITY_KB, floor_kb)


def as_int(value: str | None) -> int | None:
    if value is None or not value.isdigit():
        return None
    return int(value)


def held_bytes(record: dict[str, str]) -> int | None:
    """What an action held: the larger of its cgroup peak and sampled anon peak."""
    peak = as_int(record.get("peak_bytes"))
    if peak is None:
        return None
    return max(peak, as_int(record.get("anon_peak_bytes")) or 0)


def settled(
    current: tuple[int, int], cpu_count: int, memory_kb: int, floor_bytes: int = 0
) -> tuple[int, int]:
    """The request to make: the one in force unless the new one really differs.

    `current` is the `(cpu_count, memory_kb)` in force. It stays when the CPU
    is unchanged, the memory moves by less than `MEMORY_HYSTERESIS_KB`, and it
    still covers `floor_bytes`, the peak the rule may not go below.
    """
    if (
        current[0] == cpu_count
        and abs(current[1] - memory_kb) < MEMORY_HYSTERESIS_KB
        and current[1] * 1024 >= floor_bytes
    ):
        return current
    return cpu_count, memory_kb


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




def collect(
    lines,
    crates: set[tuple[str, str]],
    optimized_ops: frozenset[str] = frozenset(),
    clippy: bool = False,
) -> dict[str, Samples]:
    """Keeps the Lash compile samples; every other record is skipped.

    `optimized_ops` names the operations the event logs showed to be
    optimized-configuration compiles. They are sized by their own table and
    would otherwise price the dev row at an optimized peak. Clippy runs on its
    own target: `clippy=True` keeps only its records, otherwise they are
    skipped.

    The log is append-only from many actions at once, so a torn or unknown line
    is skipped rather than fatal: the table is a measurement, not a ledger.
    """
    measured: dict[str, Samples] = collections.defaultdict(Samples)
    for line in lines:
        record = parse_record(line)
        if record is None or record.get("tool") not in COMPILE_TOOLS:
            continue
        pair = (record.get("pkg", ""), record.get("crate", ""))
        if pair not in crates or record.get("op", "-") in optimized_ops:
            continue
        if (record.get("category") == "clippy") != clippy:
            continue
        observe_compile(measured[f"{pair[0]}/{pair[1]}"], record)
    return {key: samples for key, samples in measured.items() if samples.records}


def observe_compile(samples: Samples, record: dict[str, str]) -> None:
    """Adds one compile record when it is a sample: successful, a second long."""
    if record.get("exit") != "0":
        return
    wall_ms = as_int(record.get("wall_ms"))
    cpu_usec = as_int(record.get("cpu_usec"))
    peak_bytes = held_bytes(record)
    if wall_ms is None or cpu_usec is None or peak_bytes is None:
        return
    if wall_ms < MIN_WALL_MS:
        return
    samples.observe(
        cpu_usec / 1000 / wall_ms,
        peak_bytes,
        as_int(record.get("requested_cpu")) or DEFAULT_CPU_COUNT,
        as_int(record.get("requested_kb")) or 0,
    )


def collect_kinds(labelled, shared: set[str]) -> dict[tuple[str, str], Samples]:
    """The matched records of each shared identity, per `(key, kind)`.

    `labelled` is `action_categories_from_events.labelled_records`; `shared`
    names the identities a library or binary shares with its unit test.
    """
    from action_categories_from_events import TARGET_KINDS

    measured: dict[tuple[str, str], Samples] = collections.defaultdict(Samples)
    for key, kind, category, _emit, optimized, record in labelled:
        if category == "clippy":
            continue
        if key in shared and kind in TARGET_KINDS and not optimized:
            observe_compile(measured[(key, TARGET_KINDS[kind])], record)
    return {group: samples for group, samples in measured.items() if samples.records}


def collect_optimized(labelled) -> dict[tuple[str, str], Samples]:
    """The matched records of optimized-configuration compiles, per `(key, kind)`."""
    from action_categories_from_events import TARGET_KINDS

    measured: dict[tuple[str, str], Samples] = collections.defaultdict(Samples)
    for key, kind, category, _emit, optimized, record in labelled:
        if optimized and kind in TARGET_KINDS and category != "clippy":
            observe_compile(measured[(key, TARGET_KINDS[kind])], record)
    return {group: samples for group, samples in measured.items() if samples.records}


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
# `//crates/lash:lash__unit_test` then grew into that 3 GiB floor: on
# 2026-10-01 its runs held 2.75 GiB of anonymous memory, peaked at 2.9 to 3.0
# GiB, and one in eight was OOM-killed at the request. The floor is the row
# the rule gives those peaks, so its feature variants, which run too rarely to
# measure, follow it.
TEST_RUN_MINIMUM_MEMORY_KB = {
    "//crates/lash:lash__unit_test": 3840 * 1024,
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
        peak_bytes = held_bytes(record)
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


def test_run_table(
    measured: dict[str, TestRuns], current: dict[str, dict] | None = None
) -> dict[str, dict[str, float | int]]:
    """The test-run rows. `current` is the table in force, pruned of dead labels."""
    current = current or {}
    sizes = {}
    for label in sorted(set(measured) | set(TEST_RUN_MINIMUM_MEMORY_KB) | set(current)):
        runs = measured.get(label, TestRuns())
        minimum = TEST_RUN_MINIMUM_MEMORY_KB.get(
            label, TEST_RUN_MINIMUM_MEMORY_KB.get(label.split("__fv_", 1)[0])
        )
        if runs.count < TEST_MIN_SAMPLES and minimum is None:
            if label in current:
                sizes[label] = with_headroom(current[label])
            continue
        p95 = (
            percentile(runs.timed.cpu_basis(), CPU_PERCENTILE)
            if runs.timed.records
            else 0.0
        )
        p99_peak_bytes = percentile(runs.peaks, MEMORY_PERCENTILE) if runs.peaks else 0
        cpu_count = cpu_count_for(p95, within_share=False)
        memory_kb = max(
            memory_kb_for(runs.needs, floor_kb=TEST_MEMORY_FLOOR_KB)
            if runs.peaks
            else TEST_MEMORY_FLOOR_KB,
            minimum or 0,
        )
        if label in current:
            cpu_count, memory_kb = settled(
                (current[label]["cpu_count"], current[label]["memory_kb"]),
                cpu_count,
                memory_kb,
                max(math.floor(p99_peak_bytes / HEADROOM) + 1, (minimum or 0) * 1024),
            )
        sizes[label] = {
            "cpu_count": cpu_count,
            "memory_kb": memory_kb,
            "p95_cores": round(p95, 2),
            "p99_peak_bytes": p99_peak_bytes,
            "peak_bytes": runs.peak_bytes,
            "samples": runs.count,
        }
    return dict(sorted(sizes.items()))


def with_headroom(row: dict) -> dict:
    """A kept test-run row, raised when its own p99 peak crowds its request."""
    peak_bytes = row.get("p99_peak_bytes", 0)
    if peak_bytes < HEADROOM * row["memory_kb"] * 1024:
        return row
    wanted = math.ceil(peak_bytes * MEMORY_MARGIN / 1024 / MEMORY_GRANULARITY_KB)
    return dict(row, memory_kb=wanted * MEMORY_GRANULARITY_KB)


def request_of(row: dict | None) -> tuple[int, int]:
    """A row's `(cpu_count, memory_kb)`; no row is the default request."""
    row = row or {}
    return (
        row.get("cpu_count", DEFAULT_CPU_COUNT),
        row.get("memory_kb", DEFAULT_MEMORY_KB),
    )


def compile_row(
    samples: Samples,
    current: tuple[int, int],
    minimum: int = 0,
    floor_kb: int = DEFAULT_MEMORY_KB,
) -> dict[str, float | int]:
    """The row the compile rule gives `samples`, settled against `current`."""
    p95 = percentile(samples.cpu_basis(), CPU_PERCENTILE) if samples.records else 0.0
    peak_bytes = samples.peak_bytes() if samples.records else 0
    memory_kb = max(
        memory_kb_for(samples.needs, floor_kb, peak_bytes) if samples.records else 0,
        minimum,
    )
    cpu_count, memory_kb = settled(
        current, cpu_count_for(p95), memory_kb, max(peak_bytes, minimum * 1024)
    )
    return {
        "cpu_count": cpu_count,
        "memory_kb": memory_kb,
        "p95_cores": round(p95, 2),
        "p99_peak_bytes": (
            percentile(samples.peaks(), MEMORY_PERCENTILE) if samples.records else 0
        ),
        "peak_bytes": peak_bytes,
        "samples": len(samples.records),
    }


def table(
    measured: dict[str, Samples], current: dict[str, dict] | None = None
) -> dict[str, dict[str, float | int]]:
    """The crate rows. `current` is the table in force.

    Callers pass `current` already pruned of crates that no longer exist: an
    unmeasured row is kept, so a dead one would otherwise never leave.
    """
    current = current or {}
    sizes = {}
    for key in sorted(set(measured) | set(MINIMUM_MEMORY_KB) | set(current)):
        samples = measured.get(key, Samples())
        minimum = MINIMUM_MEMORY_KB.get(key)
        if len(samples.records) < MIN_SAMPLES and minimum is None:
            if key in current:
                sizes[key] = current[key]
            continue
        row = compile_row(samples, request_of(current.get(key)), minimum or 0)
        if request_of(row) == (DEFAULT_CPU_COUNT, DEFAULT_MEMORY_KB):
            continue
        sizes[key] = row
    return dict(sorted(sizes.items()))


def clippy_table(
    measured: dict[str, Samples], current: dict[str, dict] | None = None
) -> dict[str, dict[str, float | int]]:
    """The Clippy rows: the compile rule over Clippy records, 512 MiB at least.

    A crate without a row runs Clippy at its compile request, so its first row
    is not settled against anything; a row in force moves as a compile row
    does. An unmeasured row is kept, as in `table`.
    """
    current = current or {}
    sizes = {}
    for key in sorted(set(measured) | set(current)):
        samples = measured.get(key, Samples())
        if len(samples.records) < MIN_SAMPLES:
            if key in current:
                sizes[key] = current[key]
            continue
        in_force = request_of(current[key]) if key in current else (0, 0)
        sizes[key] = compile_row(samples, in_force, floor_kb=CLIPPY_FLOOR_KB)
    return sizes


def kind_table(
    measured: dict[tuple[str, str], Samples],
    crate_rows: dict[str, dict],
    current: dict[str, dict[str, dict]] | None = None,
) -> dict[str, dict[str, dict[str, float | int]]]:
    """The per-target-kind rows of the identities two targets share.

    A kind's request in force is its row in `current`, else the crate's row
    in `crate_rows`. A measured kind gets a row only when the rule moves it
    off that request; an unmeasured kind keeps whatever row it had.
    """
    current = current or {}
    sizes: dict[str, dict[str, dict]] = collections.defaultdict(dict)
    for key, rows in current.items():
        for kind, row in rows.items():
            sizes[key][kind] = row
    for (key, kind), samples in sorted(measured.items()):
        if len(samples.records) < MIN_SAMPLES:
            continue
        existing = current.get(key, {}).get(kind)
        in_force = request_of(existing or crate_rows.get(key))
        row = compile_row(samples, in_force)
        if existing is None and request_of(row) == in_force:
            continue
        sizes[key][kind] = row
    return {key: dict(sorted(rows.items())) for key, rows in sorted(sizes.items()) if rows}


def resolved_request(
    key: str, kind: str, crate_rows: dict[str, dict], kind_rows: dict[str, dict[str, dict]]
) -> tuple[int, int]:
    """What a dev-profile target of this kind asks for, as the generator resolves it."""
    return request_of(kind_rows.get(key, {}).get(kind) or crate_rows.get(key))


def optimized_table(
    measured: dict[tuple[str, str], Samples],
    dev_request,
    previous_request,
    current: dict[str, dict[str, dict]] | None = None,
) -> dict[str, dict[str, dict[str, float | int]]]:
    """The optimized-profile rows: what such a compile asks for above dev.

    `dev_request(key, kind)` is the dev request this refresh resolves and
    `previous_request(key, kind)` the one in force before it. An optimized
    compile takes the larger of its dev request and its row here, so a row
    only ever raises:

    * A measured kind's row is the compile rule over its optimized samples.
      Optimized builds are rare, so one sample is evidence, but fewer than
      MIN_SAMPLES can only raise the row in force, never lower it.
    * A dev request this refresh lowers was what the optimized compile ran
      under until now. Without MIN_SAMPLES optimized samples there is no
      evidence it needs less, so its row keeps at least that request.
    * A row no larger than the dev request says nothing and is dropped.
    """
    current = current or {}
    groups = set(measured) | {(key, kind) for key, rows in current.items() for kind in rows}
    sizes: dict[str, dict[str, dict]] = collections.defaultdict(dict)
    for key, kind in sorted(groups):
        samples = measured.get((key, kind), Samples())
        existing = current.get(key, {}).get(kind)
        dev = dev_request(key, kind)
        in_force = request_of(existing) if existing else previous_request(key, kind)
        row = compile_row(samples, in_force) if samples.records else dict(existing or {})
        if len(samples.records) < MIN_SAMPLES:
            row["cpu_count"] = max(row.get("cpu_count", 0), in_force[0])
            row["memory_kb"] = max(row.get("memory_kb", 0), in_force[1])
        if row["cpu_count"] > dev[0] or row["memory_kb"] > dev[1]:
            sizes[key][kind] = {
                "cpu_count": max(row["cpu_count"], dev[0]),
                "memory_kb": max(row["memory_kb"], dev[1]),
                "p95_cores": row.get("p95_cores", 0.0),
                "p99_peak_bytes": row.get("p99_peak_bytes", 0),
                "peak_bytes": row.get("peak_bytes", 0),
                "samples": len(samples.records),
            }
    return {key: dict(sorted(rows.items())) for key, rows in sorted(sizes.items())}


def category_table(lines, crates: set[tuple[str, str]]) -> dict[str, dict[str, int]]:
    """Per Buck2 category: its records, and the peak of those no row sizes.

    A record with a first-party Cargo identity runs under its target's row
    and is held to that row by the size tables. Every other record of a
    category -- a helper, a build script, a third-party compile -- runs under
    a default request; `unsized_peak_bytes` is the most any of them held.
    Test runs carry no category and are sized per label.
    """
    peaks: dict[str, list[int]] = collections.defaultdict(list)
    unsized: dict[str, int] = collections.defaultdict(int)
    for line in lines:
        record = parse_record(line)
        if record is None or record.get("category", "-") in ("-", ""):
            continue
        peak_bytes = held_bytes(record)
        if peak_bytes is None:
            continue
        category = record["category"]
        peaks[category].append(peak_bytes)
        if (record.get("pkg", ""), record.get("crate", "")) not in crates:
            unsized[category] = max(unsized[category], peak_bytes)
    return {
        category: {
            "p99_peak_bytes": percentile(values, MEMORY_PERCENTILE),
            "peak_bytes": max(values),
            "samples": len(values),
            "unsized_peak_bytes": unsized[category],
        }
        for category, values in sorted(peaks.items())
    }


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

    A record names its Buck2 category; one from before the supervisor wrote
    that field is grouped by what it could see then: argv[0], the Cargo
    identity and the test label. A target's actions share, by Buck2's one
    execution platform per target, its request.
    """
    tool = record.get("tool")
    if tool in BAZEL_TEST_TOOLS or tool == "process_wrapper":
        return None
    category = record.get("category", "-")
    if category not in ("-", ""):
        if category != "rustc":
            return category
        first_party = (record.get("pkg", "-"), record.get("crate", "-")) in crates
        return "rustc, first-party" if first_party else "rustc, other"
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
        fields = [as_int(record.get(name)) for name in ("wall_ms", "cpu_usec")]
        fields += [held_bytes(record)]
        fields += [as_int(record.get(name)) for name in ("requested_cpu", "requested_kb")]
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


def refresh(lines, crates: set[tuple[str, str]], inventory: dict, events: list) -> None:
    """Rewrites every size file from the usage `lines` and Buck2 `events`."""
    import action_categories_from_events as joined

    def stored(name: str) -> dict:
        return json.loads((ROOT / "tools/buck2" / name).read_text(encoding="utf-8"))

    keys = {f"{package}/{crate}" for package, crate in crates}
    labels = test_labels(inventory)
    labelled = []
    if events:
        actions = [
            action for path in events for action in joined.executed_actions(joined.event_lines(path))
        ]
        labelled = list(joined.labelled_records(actions, lines, inventory))
    optimized_ops = frozenset(
        record["op"]
        for _key, _kind, _category, _emit, optimized, record in labelled
        if optimized and record.get("op", "-") != "-"
    )
    crate_rows = table(
        collect(lines, crates, optimized_ops),
        {key: row for key, row in stored("action-sizes.json").items() if key in keys},
    )
    shared = joined.shared_identities(inventory)
    kind_rows = {
        key: rows for key, rows in stored("target-kind-sizes.json").items() if key in shared
    }
    previous = (stored("action-sizes.json"), stored("target-kind-sizes.json"))
    if events:
        kind_rows = kind_table(collect_kinds(labelled, shared), crate_rows, kind_rows)
    # Every dev request this refresh lowers keeps its optimized compile where
    # it was; see `optimized_table`.
    optimized_current = {
        key: dict(rows) for key, rows in stored("optimized-sizes.json").items() if key in keys
    }
    for key in sorted(set(previous[0]) | set(previous[1]) | set(crate_rows) | set(kind_rows)):
        if key not in keys:
            continue
        for kind in ("target", "test"):
            before = resolved_request(key, kind, *previous)
            after = resolved_request(key, kind, crate_rows, kind_rows)
            if after[0] < before[0] or after[1] < before[1]:
                optimized_current.setdefault(key, {}).setdefault(
                    kind, {"cpu_count": before[0], "memory_kb": before[1]}
                )
    optimized_rows = optimized_table(
        collect_optimized(labelled),
        lambda key, kind: resolved_request(key, kind, crate_rows, kind_rows),
        lambda key, kind: resolved_request(key, kind, *previous),
        optimized_current,
    )
    clippy_rows = clippy_table(
        collect(lines, crates, optimized_ops, clippy=True),
        {key: row for key, row in stored("clippy-sizes.json").items() if key in keys},
    )
    test_rows = test_run_table(
        collect_test_runs(lines, labels),
        {label: row for label, row in stored("test-run-sizes.json").items() if label in labels},
    )
    for name, rows in (
        ("action-sizes.json", crate_rows),
        ("target-kind-sizes.json", kind_rows),
        ("optimized-sizes.json", optimized_rows),
        ("clippy-sizes.json", clippy_rows),
        ("category-sizes.json", category_table(lines, crates)),
        ("test-run-sizes.json", test_rows),
    ):
        (ROOT / "tools/buck2" / name).write_text(render(rows), encoding="utf-8")


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
        "--events",
        action="append",
        nargs="+",
        default=[],
        type=pathlib.Path,
        metavar="EVENT_LOG",
        help="Buck2 event logs (*.pb.zst) or saved `buck2 log show` output that "
        "tell a library's actions from its unit-test binary's; with --refresh",
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
        refresh(lines, crates, inventory, [path for group in args.events for path in group])
        return subprocess.run(
            [sys.executable, str(ROOT / "tools/buck2/sync.py")], cwd=ROOT
        ).returncode
    if args.test_runs:
        labels = test_labels(inventory)
        in_force = json.loads((ROOT / "tools/buck2/test-run-sizes.json").read_text(encoding="utf-8"))
        rendered = render(
            test_run_table(
                collect_test_runs(lines, labels),
                {label: row for label, row in in_force.items() if label in labels},
            )
        )
    else:
        crates = first_party_crates(cargo_metadata())
        in_force = json.loads((ROOT / "tools/buck2/action-sizes.json").read_text(encoding="utf-8"))
        rendered = render(
            table(
                collect(lines, crates),
                {key: row for key, row in in_force.items() if tuple(key.split("/", 1)) in crates},
            )
        )
    if args.output is None:
        sys.stdout.write(rendered)
    else:
        args.output.write_text(rendered, encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
