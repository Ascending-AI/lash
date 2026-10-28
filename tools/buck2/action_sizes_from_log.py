#!/usr/bin/env python3
"""Turn the pool's per-action usage logs into `tools/buck2/action-sizes.json`.

Every remote action carries a `cpu_count` and a `memory_kb` request. Both are
part of the action key, so the defaults in `tools/buck2/platforms.bzl` are the small
action (1 CPU, 512 MiB) and move only deliberately; a compile that needs more
says so per target, from this measured table.

The input is the usage log every pool worker appends to, one tab-separated
record per action (`ACTION_USAGE_LOG`, written by the executor's action
supervisor):

    <ts>  crate=<CARGO_CRATE_NAME>  pkg=<CARGO_PKG_NAME>  kind=<...>
          tool=<argv[0]>  peak_bytes=<memory.peak>  cpu_usec=<cpu time>
          wall_ms=<wall time>  exit=<status>  requested_kb=<...>  requested_cpu=<...>
          test=<...>  op=<...>  category=<Buck2 category>  anon_peak_bytes=<...>
          action_digest=<...>  worker_cpu_count=<...>  worker_max_inflight=<...>
          worker_memory_kb=<advertised worker memory>

Copy the logs off the workers (`/workspace/kiln-executor/usage/actions.log*`)
and refresh every size file with one command:

    python3 tools/buck2/action_sizes_from_log.py --refresh \\
      --events buck-out/kiln/log/*_events.pb.zst -- usage-*.log

That rewrites `tools/buck2/action-sizes.json`,
`tools/buck2/target-kind-sizes.json`, `tools/buck2/optimized-sizes.json`,
`tools/buck2/clippy-sizes.json`, `tools/buck2/category-sizes.json` and
`tools/buck2/test-run-sizes.json` and runs `tools/buck2/sync.py`, which
regenerates `tools/buck2/exec_sizes.bzl` (requests and pool budgets) from them.
Compile rows' `peak_bytes` and `p99_peak_bytes` describe the sizing basis:
anonymous memory where available, cgroup memory for legacy records. The separate
`compile-memory-evidence.json` includes default rows and failed actions, so OOM
records are never lost when sizing filters out unsuccessful compiles.
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
supervisor, so records may say `kind=-`. Full-build compile and metadata actions share the Cargo identity and
reservation. Check commands select metadata twins sized by joined phase, target
kind and profile in `metadata-sizes.json`; an unmeasured phase keeps its full
compile request. One row per crate remains the full-build sizing key. Clippy runs on a target of its own and is sized apart
(see "Clippy" below), so a record of category `clippy` is never a compile
sample.

The rule, per `<package>/<crate>`:

* `cpu_count` = 1 while p95(cpu time / wall time / worker slot share) is
  at most 1; otherwise ceil(p95 cores - 0.2), capped at 8. The slot share is
  that sampled worker's advertised CPU count / max_inflight (historically
  1.25 on i9, 1.67 on 02). Compile samples require both metadata fields above;
  legacy logs can supply them with `--worker-metadata`, a JSON object keyed
  by log path whose values hold `worker_cpu_count` and `worker_max_inflight`.
  Missing metadata refuses sizing (S1); no pool-wide share is assumed.
* `memory_kb` = p99 anonymous peak x 1.25 plus 512 MiB, rounded up to
  256 MiB and at least the 1.5 GiB default and `MINIMUM_MEMORY_KB` floor.
  Anonymous peaks are sampled every 250 ms. The fixed allowance covers
  sampling error and the working set of file-backed pages. Older records
  without anonymous measurements keep the previous cgroup-peak rule.
* An OOM retains the maximum observed total peak across every attempt,
  including scheduler PSS for a killed supervisor. Its minimum is that peak
  x 1.25 plus 512 MiB, rounded up to 256 MiB. A request ceiling at which every
  recorded attempt was killed adds a further 512 MiB step and reserves at
  least that failed ceiling plus the step. Even a low sampled peak advances
  a failed ceiling by 256 MiB. OOM rows never lower the in-force memory
  request and bypass the successful-sample minimum and hysteresis.
  They require `worker_memory_kb` and cap at the smallest advertised budget
  of the sampled workers. CPU still needs successful samples; kills are not
  CPU samples. Existing kind/profile/metadata overrides inherit the crate's
  OOM minimum, since an unjoined kill cannot identify a cheaper phase.
* At least 20 successful samples, or the row in force stays -- a crate in
  `MINIMUM_MEMORY_KB` keeps its floor even on too few or too small samples.
  Fewer samples are not enough for a p95.
* A request is a platform property and so part of the action key: resizing a
  row re-executes every action of its targets on a cold cache. A refresh
  therefore keeps the row in force unless the request moves by a whole CPU or
  by at least 512 MiB. Anonymous p99 must remain below 80% of the request.
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

Metadata checks
---------------

`--metadata-only --events ... -o tools/buck2/metadata-sizes.json` generates
phase requests; run `kiln sync` afterwards. `--phase-report --events ...`
reports per-crate phase peaks, wall time, failures and kills. Only unambiguously
joined metadata/check actions supply the phase table, separated by target
kind and dev/optimized profile. Twenty successful samples are required. The
anonymous compile formula, fixed 512 MiB allowance, rounding and hysteresis
apply with a 512 MiB admission floor; executor cgroup floors stay unchanged.
OOM evidence uses the same step-up, including killed-only rows. Missing rows
inherit full compile requests.

Clippy
------

Every generated Rust target has a Clippy twin, `<label>__clippy`: the same
rule and attributes on an execution platform of its own, because Buck2 gives a
target one platform and Clippy needs a fraction of a compile's memory.
`tools/buck2/clippy-sizes.json` carries its request, keyed `<package>/<crate>`
from the records of category `clippy` (a library's and its unit-test binary's
Clippy share the identity, so the row covers the heavier). The rule is the
compile rule above with a 512 MiB floor (`CLIPPY_FLOOR_KB`) in place of the
1.5 GiB default. A crate without a row requests 512 MiB for Clippy.
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
  repository's labels never match. A batch's measured row prices the whole
  batch; only an unmeasured batch reserves its largest members side by side
  (see `batch_budget` in the generator).
* At least 3 samples, or no row -- except for a label in
  `TEST_RUN_MINIMUM_MEMORY_KB`, whose floor keeps its row even on too few
  samples. An unmeasured test keeps the request the generator gives it
  without one.
* A label with at least three Buck2 runs is measured from those alone. A
  label Buck2 has run less often also counts its Bazel-era runs
  (`tool=test-setup.sh`): the wrapper differed, the libtest binary and its
  cgroup did not.
* `memory_kb` = p99 anonymous peak x 1.25, rounded up to 256 MiB,
  with a 256 MiB floor and the explicit per-label floors below. Anonymous
  rows move on every refresh. A killed run's total peak x 1.25 supplies a
  minimum even when successful runs were warm. Legacy records without an
  anonymous measurement retain cgroup sizing and a 1 GiB floor.
* `cpu_count` reserves sustained demand: ceil(total CPU / total wall - 0.2),
  at least 1, over successful samples of at least one second. It also reserves
  at least ceil(p95 cores / 2), so p95 fits the executor's twofold CPU burst.
  The request is capped at 8. Short parallel selections must not price every
  long serial selection; weighting by wall time also prevents many short
  serial runs from hiding a long parallel run. Timing-sensitive suites retain
  their separate contention floors. A test whose every run is shorter asks
  for one core: it cannot show more, and holds it for under a second.
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

# Measured compile sizing retains its existing baseline. Admission defaults
# live in generate_model.py and may be below the executor's cgroup floor.
DEFAULT_MEMORY_KB = 1572864
UNMEASURED_MEMORY_KB = 512 * 1024
# The executor raises every cgroup to at least this limit, independently of admission.
ACTION_MEMORY_FLOOR_KB = 2097152
DEFAULT_CPU_COUNT = 1
# The least a measured Clippy row asks for: Clippy's median peak is under
# 100 MiB, and a smaller floor would only register more execution platforms.
# Must match `CLIPPY_FLOOR_KB` in `tools/buck2/generate_model.py`.
CLIPPY_FLOOR_KB = 512 * 1024

COMPILE_TOOLS = {"python3", "rustdoc"}
MIN_WALL_MS = 1000
MIN_SAMPLES = 20
CPU_PERCENTILE = 95
# How far above a whole core the compile p95 or sustained test demand may
# sit and still round down.
CPU_TOLERANCE = 0.2
# The pool caps a single action's request at 8 cores.
MAX_CPU_COUNT = 8
# The executor gives each reserved CPU proportional weight and a twofold
# cpu.max burst. Admission prices sustained test work; the p95 guard keeps
# parallel runs within that execution ceiling (FIG-6619).
TEST_CPU_BURST = 2
# The proportional margin covers variation between inputs; compile anonymous
# samples also get a fixed allowance for file-backed pages and sampling error.
# Test runs and legacy compiles keep `memory_need`'s previous request bound.
MEMORY_MARGIN = 1.25
COMPILE_CACHE_BYTES = 512 * 1024 * 1024
COMPILE_HEADROOM = 0.8
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
# Fully censored request ceilings need a full resize step beyond sampled need.
COMPILE_OOM_STEP_KB = MEMORY_HYSTERESIS_KB
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
    "lash-internal-core-execution/lash_core_execution": 2 * 1024 * 1024,
    "lash-internal-core-execution/store_backed": 2 * 1024 * 1024,
    "lash-internal-core/runtime_turns": 2 * 1024 * 1024,
    "lash-internal-core-worker/lash_core_worker": 2 * 1024 * 1024,
}


class Samples:
    """Every kept measurement for one `<package>/<crate>` key."""

    def __init__(self) -> None:
        # (cores, peak_bytes, requested_cpu)
        self.records: list[tuple[float, int, int]] = []
        self.needs: list[float] = []
        self.legacy_peaks: list[int] = []
        self.anon_peaks: list[int] = []
        self.oom_kills = 0
        self.slot_ratios: list[float] = []
        self.attempts: collections.Counter[int] = collections.Counter()
        self.killed_requests: collections.Counter[int] = collections.Counter()
        self.observed_peak_bytes = 0
        self.worker_memory_kb: int | None = None

    def observe(
        self,
        cores: float,
        peak_bytes: int,
        requested_cpu: int,
        requested_kb: int = 0,
        anon_peak_bytes: int | None = None,
        slot_share: float | None = None,
    ) -> None:
        basis = peak_bytes if anon_peak_bytes is None else anon_peak_bytes
        self.observed_peak_bytes = max(self.observed_peak_bytes, peak_bytes)
        self.records.append((cores, basis, requested_cpu))
        if slot_share is not None:
            self.slot_ratios.append(cores / slot_share)
        if anon_peak_bytes is None:
            self.legacy_peaks.append(peak_bytes)
            self.needs.append(memory_need(peak_bytes, requested_kb))
        else:
            self.anon_peaks.append(anon_peak_bytes)
            self.needs.append(anon_peak_bytes * MEMORY_MARGIN + COMPILE_CACHE_BYTES)

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


def cpu_count_for(p95_cores: float, *, slot_share: float | None = None) -> int:
    """The request above a compile slot share, or for sustained test demand."""
    if slot_share is not None and p95_cores <= slot_share:
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
                crates.add((package["name"], "build_script_build"))
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
        if pair not in crates or (not clippy and record.get("op", "-") in optimized_ops):
            continue
        if (record.get("category") == "clippy") != clippy:
            continue
        observe_compile(measured[f"{pair[0]}/{pair[1]}"], record)
    return {key: samples for key, samples in measured.items() if samples.records or samples.oom_kills}


def observe_compile(samples: Samples, record: dict[str, str]) -> None:
    """Keep sizing samples and retain kills even when they leave no sample."""
    if record.get("exit") in (None, "", "-"):
        return
    killed = oom_killed(record)
    requested_kb = as_int(record.get("requested_kb")) or 0
    budget_kb = as_int(record.get("worker_memory_kb"))
    if killed and (not requested_kb or not budget_kb):
        raise ValueError("OOM sizing requires requested_kb and worker_memory_kb")
    if budget_kb:
        samples.worker_memory_kb = min(samples.worker_memory_kb or budget_kb, budget_kb)
    samples.attempts[requested_kb] += 1
    samples.observed_peak_bytes = max(samples.observed_peak_bytes, held_bytes(record) or 0)
    samples.oom_kills += killed
    if killed:
        samples.killed_requests[requested_kb] += 1
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
        as_int(record.get("anon_peak_bytes")),
        worker_slot_share(record),
    )


def worker_slot_share(record: dict[str, str]) -> float:
    """S1: no compile sizing without the sampled worker's advertised share."""
    cpus = as_int(record.get("worker_cpu_count"))
    slots = as_int(record.get("worker_max_inflight"))
    if not cpus or not slots:
        raise ValueError(
            "compile sizing requires worker metadata: worker_cpu_count and "
            "worker_max_inflight on each record, or --worker-metadata for each log"
        )
    return cpus / slots


def oom_killed(record: dict[str, str]) -> bool:
    """Explicit OOM counts, or SIGKILL exits conservatively treated as OOM.

    The Rust wrapper returns -9 through Python's sys.exit, which the shell
    observes as 247. Killing the wrapper itself yields 137. Usage logs do
    not distinguish these kills from an external SIGKILL.
    """
    return (
        bool(as_int(record.get("oom_kill")) or as_int(record.get("oom_kills")))
        or record.get("exit") in {"-9", "137", "247"}
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
    return {group: samples for group, samples in measured.items() if samples.records or samples.oom_kills}


def collect_optimized(labelled) -> dict[tuple[str, str], Samples]:
    """The matched records of optimized-configuration compiles, per `(key, kind)`."""
    from action_categories_from_events import TARGET_KINDS

    measured: dict[tuple[str, str], Samples] = collections.defaultdict(Samples)
    for key, kind, category, _emit, optimized, record in labelled:
        if optimized and kind in TARGET_KINDS and category != "clippy":
            observe_compile(measured[(key, TARGET_KINDS[kind])], record)
    return {group: samples for group, samples in measured.items() if samples.records or samples.oom_kills}


# The prelude names metadata-fast with infallible diagnostics `diag`.
METADATA_EMITS = {"metadata", "metadata-fast", "check", "diag"}


def collect_metadata(labelled) -> dict[tuple[str, str, bool], Samples]:
    """Only joined metadata actions, separated by target kind and profile.

    Unjoined records cannot prove a phase and never lower its reservation.
    Killed actions remain evidence even when no successful sample survives.
    """
    from action_categories_from_events import TARGET_KINDS

    measured = collections.defaultdict(Samples)
    for key, kind, category, emit, optimized, record in labelled:
        if category == "rustc" and emit in METADATA_EMITS and kind in TARGET_KINDS:
            observe_compile(measured[(key, TARGET_KINDS[kind], optimized)], record)
    return dict(measured)


def metadata_table(measured, current, inherited) -> dict:
    """Measured phase requests; missing rows inherit full compile requests.

    Metadata uses the anonymous compile formula and allowance, with the
    512 MiB admission floor used by Clippy. Its cgroup floor is unchanged.
    Twenty samples and the compile hysteresis still apply to successful-only
    evidence. OOMs use the compile step-up even without successful samples.
    """
    rows = {profile: {key: dict(kinds) for key, kinds in current.get(profile, {}).items()}
            for profile in ("dev", "optimized")}
    for (key, kind, optimized), samples in sorted(measured.items()):
        if len(samples.records) < MIN_SAMPLES and not samples.oom_kills:
            continue
        profile = "optimized" if optimized else "dev"
        previous = rows[profile].get(key, {}).get(kind)
        request = request_of(previous) if previous else inherited(key, kind, optimized)
        row = compile_row(samples, request, floor_kb=CLIPPY_FLOOR_KB)
        row.update(compile_evidence({key: samples})[key])
        check_compile_memory(row, row["memory_kb"], key)
        rows[profile].setdefault(key, {})[kind] = row
    return rows


def phase_report(labelled) -> dict:
    """Per-crate phase peaks and time, retaining failed and killed actions."""
    groups = collections.defaultdict(list)
    for key, kind, category, emit, optimized, record in labelled:
        phase = "metadata" if category == "rustc" and emit in METADATA_EMITS else emit or category
        groups[(key, kind, "optimized" if optimized else "dev", phase)].append(record)
    rows = {}
    for group, records in sorted(groups.items()):
        successes = [r for r in records if r.get("exit") == "0"]
        peaks = [held_bytes(r) for r in records if held_bytes(r) is not None]
        anon = [as_int(r.get("anon_peak_bytes")) for r in successes]
        anon = [peak for peak in anon if peak is not None]
        rows["|".join(group)] = {
            "actions": len(records), "successful": len(successes),
            "failed": len(records) - len(successes),
            "oom_kills": sum(oom_killed(r) for r in records),
            "peak_bytes": max(peaks, default=0),
            "p99_anon_peak_bytes": percentile(anon, MEMORY_PERCENTILE) if anon else 0,
            "wall_ms": sum(as_int(r.get("wall_ms")) or 0 for r in records),
        }
    return rows


TEST_MIN_SAMPLES = 3
TEST_MEMORY_FLOOR_KB = 256 * 1024
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
        self.cpu_usec = 0
        self.wall_ms = 0
        self.killed_peak_bytes = 0
        self.anonymous = False

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
        self.cpu_usec += other.cpu_usec
        self.wall_ms += other.wall_ms
        self.killed_peak_bytes = max(self.killed_peak_bytes, other.killed_peak_bytes)
        self.anonymous = self.anonymous or other.anonymous

    def observe(
        self,
        cpu_usec: int,
        wall_ms: int,
        peak_bytes: int,
        requested_cpu: int,
        requested_kb: int = 0,
        anon_peak_bytes: int | None = None,
    ) -> None:
        self.anonymous = self.anonymous or anon_peak_bytes is not None
        basis = peak_bytes if anon_peak_bytes is None else anon_peak_bytes
        self.peaks.append(basis)
        self.needs.append(memory_need(peak_bytes, requested_kb) if anon_peak_bytes is None else basis * MEMORY_MARGIN)
        if wall_ms >= MIN_WALL_MS:
            self.timed.observe(cpu_usec / 1000 / wall_ms, peak_bytes, requested_cpu)
            self.cpu_usec += cpu_usec
            self.wall_ms += wall_ms


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
        if record is None:
            continue
        role, _, rest = record.get("test", "-").partition(":")
        _shard, _, label = rest.partition(":")
        if label.startswith("root//"):
            label = label.removeprefix("root")
        if role != "run" or label not in labels:
            continue
        wall_ms = as_int(record.get("wall_ms"))
        cpu_usec = as_int(record.get("cpu_usec"))
        era = legacy if record.get("tool") in BAZEL_TEST_TOOLS else measured
        if oom_killed(record):
            era[label].killed_peak_bytes = max(era[label].killed_peak_bytes, held_bytes(record) or 0)
        if record.get("exit") != "0":
            continue
        peak_bytes = as_int(record.get("anon_peak_bytes"))
        if peak_bytes is None:
            peak_bytes = held_bytes(record)
        if wall_ms is None or cpu_usec is None or peak_bytes is None:
            continue
        requested_cpu = as_int(record.get("requested_cpu")) or DEFAULT_CPU_COUNT
        era[label].observe(
            cpu_usec, wall_ms, peak_bytes, requested_cpu, as_int(record.get("requested_kb")) or 0,
            as_int(record.get("anon_peak_bytes"))
        )
    for label, runs in legacy.items():
        if measured[label].count < TEST_MIN_SAMPLES:
            measured[label].absorb(runs)
    return {label: runs for label, runs in measured.items() if runs.count or runs.killed_peak_bytes}


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
            if runs.killed_peak_bytes:
                raise ValueError(f"test run {label} was killed without enough samples; set an explicit floor")
            if label in current:
                sizes[label] = with_headroom(current[label])
            continue
        p95 = (
            percentile(runs.timed.cpu_basis(), CPU_PERCENTILE)
            if runs.timed.records
            else 0.0
        )
        p99_peak_bytes = percentile(runs.peaks, MEMORY_PERCENTILE) if runs.peaks else 0
        mean_cores = runs.cpu_usec / 1000 / runs.wall_ms if runs.wall_ms else 0.0
        cpu_count = min(
            MAX_CPU_COUNT,
            max(cpu_count_for(mean_cores), math.ceil(p95 / TEST_CPU_BURST)),
        )
        memory_kb = max(
            memory_kb_for(runs.needs, floor_kb=TEST_MEMORY_FLOOR_KB if runs.anonymous else 1024 * 1024)
            if runs.peaks
            else TEST_MEMORY_FLOOR_KB,
            minimum or 0,
            math.ceil(runs.killed_peak_bytes * MEMORY_MARGIN / 1024 / MEMORY_GRANULARITY_KB) * MEMORY_GRANULARITY_KB,
        )
        if label in current and not runs.anonymous:
            cpu_count, memory_kb = settled(
                (current[label]["cpu_count"], current[label]["memory_kb"]),
                cpu_count, memory_kb,
                max(math.floor(p99_peak_bytes / HEADROOM) + 1, memory_kb * 1024 if runs.killed_peak_bytes else (minimum or 0) * 1024),
            )
        sizes[label] = {
            "cpu_count": cpu_count,
            "memory_kb": memory_kb,
            "p95_cores": round(p95, 2),
            "mean_cores": round(mean_cores, 2),
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
        row.get("memory_kb", UNMEASURED_MEMORY_KB),
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
        memory_kb_for(samples.needs, floor_kb, max(samples.legacy_peaks, default=0))
        if samples.records else floor_kb,
        minimum,
    )
    if samples.records and len(samples.slot_ratios) != len(samples.records):
        raise ValueError("compile sizing requires worker metadata for every sample")
    cpu_count = (
        DEFAULT_CPU_COUNT
        if not samples.records or percentile(samples.slot_ratios, CPU_PERCENTILE) <= 1
        else cpu_count_for(p95)
    )
    oom = oom_evidence(samples)
    oom_floor = oom.get("oom_floor_kb", 0)
    if samples.oom_kills:
        memory_kb = max(memory_kb, current[1], oom_floor)
        if len(samples.records) < MIN_SAMPLES:
            cpu_count = max(DEFAULT_CPU_COUNT, current[0])
        if memory_kb > oom["worker_memory_kb"]:
            if current[1] > oom["worker_memory_kb"] or minimum > oom["worker_memory_kb"]:
                raise ValueError("in-force memory request exceeds sampled worker budget")
            memory_kb = oom["worker_memory_kb"]
    cpu_count, memory_kb = settled(
        current, cpu_count, memory_kb,
        max(max(samples.legacy_peaks, default=0), minimum * 1024, oom_floor * 1024),
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
        **oom,
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
        if len(samples.records) < MIN_SAMPLES and minimum is None and not samples.oom_kills:
            if key in current:
                sizes[key] = current[key]
            continue
        row = compile_row(samples, request_of(current.get(key)), minimum or 0)
        if not samples.oom_kills and request_of(row) == (DEFAULT_CPU_COUNT, UNMEASURED_MEMORY_KB):
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
        if len(samples.records) < MIN_SAMPLES and not samples.oom_kills:
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
        if len(samples.records) < MIN_SAMPLES and not samples.oom_kills:
            previous = sizes[key].get(kind)
            if previous and (
                max(samples.legacy_peaks, default=0) > previous["memory_kb"] * 1024
                or (samples.anon_peaks and percentile(samples.anon_peaks, MEMORY_PERCENTILE)
                    >= COMPILE_HEADROOM * previous["memory_kb"] * 1024)
            ):
                check_compile_memory(
                    compile_evidence({key: samples})[key],
                    request_of(crate_rows.get(key))[1], (key, kind),
                )
                sizes[key].pop(kind)
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
        row = compile_row(samples, in_force) if samples.records or samples.oom_kills else dict(existing or {})
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
                **oom_evidence(samples),
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
    packages = {package for package, _crate in crates}
    for line in lines:
        record = parse_record(line)
        if record is None or record.get("category", "-") in ("-", ""):
            continue
        peak_bytes = held_bytes(record)
        if peak_bytes is None:
            continue
        category = record["category"]
        peaks[category].append(peak_bytes)
        pair = (record.get("pkg", ""), record.get("crate", ""))
        # A retired target in a current first-party package has no action in
        # today's graph; its historical peak does not size a default action.
        if pair not in crates and pair[0] not in packages:
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
    metadata_rows = metadata_table(
        collect_metadata(labelled),
        {profile: {key: rows for key, rows in by_key.items() if key in keys}
         for profile, by_key in stored("metadata-sizes.json").items()},
        lambda key, kind, optimized: (
            request_of(optimized_rows.get(key, {}).get(kind))
            if optimized and kind in optimized_rows.get(key, {})
            else resolved_request(key, kind, crate_rows, kind_rows)
        ),
    )
    # A crate-only kill cannot name a cheaper target kind or metadata phase.
    # Existing overrides must not hide the aggregate OOM floor.
    for key, crate in crate_rows.items():
        floor = crate.get("oom_floor_kb", 0)
        if not floor:
            continue
        overrides = list(kind_rows.get(key, {}).values())
        overrides += list(optimized_rows.get(key, {}).values())
        overrides += [row for by_key in metadata_rows.values()
                      for row in by_key.get(key, {}).values()]
        for row in overrides:
            row["memory_kb"] = max(row["memory_kb"], floor)
    test_rows = test_run_table(
        collect_test_runs(lines, labels),
        {label: row for label, row in stored("test-run-sizes.json").items() if label in labels},
    )
    evidence = {
        "crates": compile_evidence(collect(lines, crates, optimized_ops)),
        "clippy": compile_evidence(collect(lines, crates, optimized_ops, clippy=True)),
        "kinds": {},
        "optimized": {},
    }
    for name, measured in (
        ("kinds", collect_kinds(labelled, shared)),
        ("optimized", collect_optimized(labelled)),
    ):
        for (key, kind), row in compile_evidence(measured).items():
            evidence[name].setdefault(key, {})[kind] = row
    (ROOT / "tools/buck2/compile-memory-evidence.json").write_text(render(evidence), encoding="utf-8")
    for name, rows in (
        ("action-sizes.json", crate_rows),
        ("target-kind-sizes.json", kind_rows),
        ("optimized-sizes.json", optimized_rows),
        ("clippy-sizes.json", clippy_rows),
        ("metadata-sizes.json", metadata_rows),
        ("category-sizes.json", category_table(lines, crates)),
        ("test-run-sizes.json", test_rows),
    ):
        (ROOT / "tools/buck2" / name).write_text(render(rows), encoding="utf-8")


def oom_evidence(samples: Samples) -> dict:
    """A censored compile's minimum request, capped by its sampled workers."""
    if not samples.oom_kills:
        return {}
    if not samples.worker_memory_kb or not samples.killed_requests:
        raise ValueError("OOM sizing requires worker memory and failed request evidence")
    all_killed = max(
        (request for request, count in samples.killed_requests.items()
         if count == samples.attempts[request]), default=0,
    )
    need = samples.observed_peak_bytes * MEMORY_MARGIN + COMPILE_CACHE_BYTES
    if all_killed:
        need += COMPILE_OOM_STEP_KB * 1024
    floor = max(
        math.ceil(need / 1024 / MEMORY_GRANULARITY_KB) * MEMORY_GRANULARITY_KB,
        max(samples.killed_requests) + MEMORY_GRANULARITY_KB,
        all_killed + COMPILE_OOM_STEP_KB if all_killed else 0,
    )
    budget = samples.worker_memory_kb // MEMORY_GRANULARITY_KB * MEMORY_GRANULARITY_KB
    return {
        "oom_kills": samples.oom_kills,
        "oom_floor_kb": min(floor, budget),
        "oom_peak_bytes": samples.observed_peak_bytes,
        "oom_max_request_kb": max(samples.killed_requests),
        "oom_all_killed_request_kb": all_killed,
        "worker_memory_kb": budget,
    }


def compile_evidence(measured: dict) -> dict:
    """Safety measurements, including defaults and failed actions."""
    return {
        key: {
            "p99_anon_peak_bytes": percentile(samples.anon_peaks, MEMORY_PERCENTILE) if samples.anon_peaks else 0,
            "legacy_peak_bytes": max(samples.legacy_peaks, default=0),
            "oom_kills": samples.oom_kills,
            "samples": len(samples.records),
            **oom_evidence(samples),
        }
        for key, samples in sorted(measured.items())
    }


def prune_compile_evidence(evidence: dict, crates: set[tuple[str, str]]) -> dict:
    """Drop retired identities, preserving all evidence for surviving targets."""
    keys = {f"{package}/{crate}" for package, crate in crates}
    return {
        profile: {key: row for key, row in rows.items() if key in keys}
        for profile, rows in evidence.items()
    }


def check_compile_memory(evidence: dict, memory_kb: int, identity: object) -> None:
    """Require OOM step-up and preserve anonymous and legacy headroom."""
    if evidence["oom_kills"]:
        assert memory_kb >= evidence["oom_floor_kb"], (identity, "OOM peak needs step-up")
        assert memory_kb <= evidence["worker_memory_kb"], (identity, "request exceeds worker budget")
    assert evidence["p99_anon_peak_bytes"] < COMPILE_HEADROOM * memory_kb * 1024, (identity, "compile p99 needs headroom")
    assert evidence["legacy_peak_bytes"] <= memory_kb * 1024, (identity, "legacy peak exceeds request")


def read_lines(
    paths: list[pathlib.Path], since: int, worker_metadata: dict | None = None
) -> list[str]:
    lines = []
    for path in paths:
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            stamp = line.partition("\t")[0]
            if since and (not stamp.isdigit() or int(stamp) < since):
                continue
            if worker_metadata is not None:
                metadata = worker_metadata.get(str(path))
                if metadata is None:
                    raise ValueError(f"missing worker metadata for {path}")
                # Metadata belongs to this captured log, not the current pool.
                for field in ("worker_cpu_count", "worker_max_inflight"):
                    line += f"\t{field}={metadata[field]}"
                if "worker_memory_kb" in metadata:
                    line += f"\tworker_memory_kb={metadata['worker_memory_kb']}"
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
    parser.add_argument("--phase-report", action="store_true", help="report per-crate phase peaks and time from --events")
    parser.add_argument("--metadata-only", action="store_true", help="generate measured metadata-sizes.json from --events; run kiln sync afterwards")
    parser.add_argument(
        "--worker-metadata", type=pathlib.Path,
        help="JSON keyed by usage-log path, with worker_cpu_count and worker_max_inflight",
    )
    args = parser.parse_args()
    worker_metadata = (
        json.loads(args.worker_metadata.read_text(encoding="utf-8"))
        if args.worker_metadata else None
    )
    lines = read_lines(args.logs, args.since, worker_metadata)
    inventory = json.loads(
        (ROOT / "tools/buck2/target-inventory.json").read_text(encoding="utf-8")
    )
    if args.phase_report or args.metadata_only:
        if not args.events:
            parser.error("phase reporting and metadata sizing require --events")
        import action_categories_from_events as joined
        import generate_model

        actions = [action for group in args.events for path in group
                   for action in joined.executed_actions(joined.event_lines(path))]
        labelled = list(joined.labelled_records(actions, lines, inventory))
        if args.phase_report:
            rendered = render(phase_report(labelled))
        else:
            keys = {key for key, _kind in joined.target_identities(inventory).values()}
            rendered = render(metadata_table(
                collect_metadata(labelled),
                {profile: {key: kinds for key, kinds in rows.items() if key in keys}
                 for profile, rows in generate_model.METADATA_SIZES.items()},
                lambda key, kind, optimized: tuple(
                    (generate_model.optimized_request if optimized else generate_model.compile_request)(
                        *key.split("/", 1), kind
                    )[field] for field in ("cpu_count", "memory_kb")
                ),
            ))
        if args.output is None:
            sys.stdout.write(rendered)
        else:
            args.output.write_text(rendered, encoding="utf-8")
        return 0
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
