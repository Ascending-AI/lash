# Interpreter comparison

`lash-perf compare` is a developer benchmark, outside the merge path. It runs
fresh TypeScript and Python kernel cells using `built_embedding`'s production
worker natives and the released helper library, just as Test262 does. Guest
memory and exact charge metering stay on by default. It does not start the
durable engine for compute measurements. The testing-only
`lash_vm_worker::core_support::compare` adapter owns lowering, linking and
fresh machines; the harness receives opaque prepared cells and measurements.
Production hosts continue to submit runs through the worker service.

`--no-charge` and `--no-memory-meter` are diagnostic ablations for attributing
accounting cost. Their receipts record the actual modes and omit disabled
bounds. Neither option can be combined with `--history`; these runs do not
establish the normal metered performance target.

Build and locate the optimized executable:

```sh
. ./env.sh
kiln build //crates/lash-perf:lash-perf__bin --config=optimized \
  --materializations final --build-report .kiln/compare-build.json
python3 tools/buck2/outputs.py --report .kiln/compare-build.json \
  --label //crates/lash-perf:lash-perf__bin --single
```

Reference paths are optional. Repeat `--qjs` for upstream QuickJS and dquickts;
receipt names `qjs-0`, `qjs-1` follow argument order and retain binary hashes.
`--node` produces both `node-jitless` and `node-jit` rows; `--python` runs CPython.
`--monty` selects the Monty CLI and requires `--python` so every Monty result
is checked directly against CPython at both trip counts in every sample.
All engines for a dialect receive the same checked-in workload source. Only
clocks, invocation and result transport wrappers differ. External startup and
JSON transport are excluded from reference timings. Lash includes fresh cell
construction and run through its printed checksum; lowering, linking, machine
destruction and host result validation are excluded. The two-point secant describes incremental work only for that exact geometry.
For nonlinear `object_sort`, the table omits the intercept because two points
do not identify fixed setup cost. Retain the actual high-count durations.

Build the pinned external references into a cache without vendoring them:

```sh
kiln gate lash <fork> -- bash -c 'ulimit -v 8388608; /usr/bin/time -v python3 scripts/build-compare-references.py --cache target/compare-references' \
  > .kiln/compare-references.log 2>&1
```

Use `--engine monty` to fetch only Monty. The fetcher reuses the embedded
instrument's archive verification and extracts only the named CLI executable.
The current pin is pydantic-monty-runtime 1.4.0, the newest PyPI release checked
on 2026-10-27, with archive SHA-256
`ff6301049a8e2f3407188dd174a749f2f2af6f82a2bc993c707dbebf44dcf651`.
It is the optimized Linux x86_64 manylinux_2_28 wheel, requiring glibc 2.28+.

The cache's `engines.json` names paths and pinned revisions, and records
Monty's URL, version, archive checksum and executable checksum. Builds use the
upstream Makefile defaults, two compiler jobs, and build dquickts' snapshot
host separately. Run measurements under the same hard memory cap and retain
RSS and instruction evidence:

```sh
kiln gate lash <fork> -- bash -c 'ulimit -v 8388608; /usr/bin/time -v perf stat -e instructions:u -- "$@"' -- \
  <optimized-lash-perf> compare --out .kiln/compare.json \
  --instructions \
  --qjs <upstream-qjs> --qjs <dquickts-qjs> --node <node> --python /usr/bin/python3 \
  --monty <cache>/monty-bin
```

Receipts record comparison geometry version 2. Ordinary comparisons open no
history file and every compute execution owns a fresh in-memory cell. Optional
durable observations claim a new private runtime directory and remove it on exit,
including errors; reusing an output path never reuses persisted runtime state.

`--instructions` opens Linux's `instructions:u` counter and reads it immediately
before and after each synchronous fresh-cell execution. It excludes embedding,
lowering/linking, diagnostic cells, executable hashing and driver/history I/O.
It includes construction, execution, destruction and result validation; its
interval therefore extends beyond the duration interval. Each sample retains
its unscaled count plus time-enabled/time-running, and rows report minimum and
median differential instructions/op with
the same two-point formula below. This scoped counter avoids subtracting huge
whole-process startup costs to measure small loop increments. Counter refusal is
an explicit comparison failure. A measured interval with less than 99% PMU
running time is refused; counts are never silently scaled.

With `--instructions`, references run a separate low/high process pair per
sample, alternating the count order. Both processes execute the same fixed
`--warmups` calls at the low count before the measured call. `perf stat
--no-scale -e instructions:u` counts each whole process; parsing, startup,
fixed warmups, output and teardown cancel to first order in the two-point
slope. Literal compilation, allocator effects and adaptive JIT behavior may
not cancel exactly. These are incremental instruction estimates, not a scoped
resident execution counter. The in-guest clock still times only the final
benchmark call. Without `--instructions`, the existing resident multi-pair
clock runner remains in use. Raw sources, stdout, stderr and perf TSV files
are retained beside the receipt. Every reference counter must run at least
99% of enabled time; missing or multiplexed counts fail the comparison.

The output starts with all selected Python workloads, sorted by decreasing
Lash/Monty instruction ratio, beside the Lash-TS/`qjs-0` instruction ratio.
Each engine retains its source and executable SHA-256. A Monty language
limitation is retained as `unsupported: <error>` with the complete CLI
traceback; it never removes a workload. Resource, transport, counter and
checksum failures still refuse the comparison.

Each workload has a low count and a high count (`low * --factor`, default 100).
One operation is the original microbench's unit (four property accesses or
calls per outer trip, ten array accesses, three integer conversions, 1000
inner arithmetic/string/map operations). Sort's unit is a complete batch of
14 seeded 1000-element shapes including setup and order validation. Python's
sort unit is one 1000-element descending-input setup/sort/validation batch.
The agent units are a 128-record JSON tool result, a filter/map/reduce over
10,000 records, and 100 rendered template lines. These different units must
never be averaged together or compared across dialects. Python uses
comprehensions and sum for filter/map/reduce; its JSON workload uses
`json.loads` and `json.dumps` with compact separators,
matching TypeScript's `JSON.parse` and `JSON.stringify` over the same records.
CPython executes that identical Python source.

There is one warmup pair by default and five measured pairs (`--warmups`,
`--runs`). The receipt retains every duration and checksum. Separate fits of
the two minima and the two medians use:

```text
slope_ns_per_operation = (t_high - t_low) / (ops_high - ops_low)
```

The adjacent Markdown table reports both secants and lash/reference ratios.
Linear workloads also retain the signed median intercept; `object_sort` has
no fixed-cost display.
Nonpositive wall secants have no ratio or wall-secant trend; instruction
history remains available when its own secant is positive. Increase trip
separation when investigating nonpositive secants. Refused execution, nondeterministic results,
invalid sample counts and mismatched reference checksums make the command fail;
partial JSON evidence survives. Numeric checksums allow a relative 1e-10 float
tolerance. Raw outputs remain archived in the receipt.

The command pins itself and its children to the first allowed CPU when
`taskset` works. `--cpu` selects another; `--no-pin` opts out. It records the
actual pin status and load averages before/after. `/usr/bin/time -v` records
peak RSS beside the receipt. Shared-host wall clocks are descriptive: collect
paired `perf stat -e instructions:u` evidence as well, without waiting for a
quiet host.

The separate diagnostic cell uses the existing perf-witness monotonic clock
for admission/lowering, linking, construction and its first pre-run checkpoint.
An additional inclusive preparation observer reports library compilation; that
number overlaps execution/construction and must not be added to the other
phases. Embedding setup is process-once, not per-execution. The diagnostic
machine's checkpoint never perturbs the ordinary slope population.

History is opt-in. Pass `--history <file>` to append observations and
`--instructions --check` to compare against a prior qualified instructions/op
baseline. The default threshold is 1% (`--tolerance 0.01`). Wall time remains
in the receipt and duration trend, but never decides this check. A missing
instruction baseline, unqualified hardware window or nonpositive instruction
secant fails. Failed checks never append a new baseline.

History uses the existing `runtime_perf/duration_trend` JSONL format, with
`compare_instructions_per_operation` stored in instruction units. Duration
metrics retain their original millisecond units. Old duration-only records
cannot qualify as instruction baselines. Host/compiler/allocator/build/CPU,
source, geometry, observation and metering identities must match. External
reference rows remain descriptive. This is an opt-in developer gate.

## Python numeric and JSON comparison

The FIG-7465 full optimized comparison on 2026-10-27 uses the supported
Python JSON module against TS `JSON.parse`/`JSON.stringify`. Both bodies
parse the same 128-record ASCII object, increment every value once, and sum
compact serialized lengths. CPython and Node independently agree at 1, 10
and 100 trips, returning 7701, 77010 and 770100. The array row retains
FIG-7466's corrected ten per-read accumulator updates.

| Workload | Python instructions/op | TS instructions/op | Python/TS |
|---|---:|---:|---:|
| float_arith | 619.372 | 608.829 | 1.017317x |
| json_transform | 5,742,611.556 | 5,862,875.444 | 0.979487x |
| array_read | 1,867.492 | 1,054.807 | 1.770458x |

All 34 Lash rows passed the 1% no-regression bound against the full main
baseline at `2d809c6b68`, with one warmup, three low/high pairs and factor 10.
All 204 measured Lash PMU windows qualified at >=99% running time. Exact
charge and memory metering were enabled; shared-host wall time is unclaimed.
The source-matched charge census has 383 executable programs, 5 lower, 378
equal and no higher charges. Full evidence is in
`/workspace/notes/lash/tasks/lanes/r33/pyjson.evidence/report.md`.

## Monty comparison

The combined FIG-7484 second-pass snapshot on 2026-10-28 uses full optimized
implementation `59de4bf4a3f7`, based on main `fc858ca10f04`.
It includes FIG-7485's resident-text installation change and FIG-7510's shared
admission implementation, plus FIG-7513's unpaid-region replay repair.
Every numeric row below comes from this snapshot.
The fresh starting baseline is `1ed410899125`.

All 85 engine/workload rows and 510 samples passed source, result, reference
identity, trip and work-unit checks. Three low/high pairs, one warmup and
factor 20 ran on CPU 0 with observation off, exact charge and held-byte
accounting, and an 8 GiB address-space cap. All 204 scoped Lash and 306 reference
PMU windows ran at 100%; all 102 Monty outputs matched CPython. No Lash row
regressed more than 1% against either the fresh baseline or the landed
FIG-7485 snapshot. Every TS/QuickJS row meets 10x. Values are median
incremental instructions/op; Python and TS retain their separate denominators.
Shared-host wall time is unclaimed. These completed receipts are reused after
clean delivery rebases under lane-mechanics.md section 3.

Collection lowering now normalizes private comprehension bindings in their
own region. JSON numbers borrow validated token spans, inline integer output
reuses the existing formatter, and member results reuse register assignment
destinations. The completed 383-source census records 14 charge reductions
and no increases; all outcomes equal the fresh baseline.

The collection rows remain above parity, so FIG-7484 stays open. Memory-off
net deltas do not explain the full gaps; changed execution censuses are
excluded from causal intrinsic-cost attribution. No complete intrinsic floor
is claimed. Current raw results, source/executable identities, counters and
correctness receipts are in
`/workspace/notes/lash/tasks/lanes/r33/pymonty-coll2.evidence/`, especially
`qualified-full.json`, `qualified-full-verification.json`, `qualified-full-assessment.json`,
`final-charge-verification.json`, `delivery-proof.json` and
`semantic-costs.json`.

| Python workload | Lash-Python instr/op | Monty instr/op | Lash/Monty | Lash-TS/QuickJS |
|---|---:|---:|---:|---:|
| object_sort | 15071.72 | 7769.01 | 1.9400x | 8.8169x |
| map_set_string | 6042.77 | 5409.96 | 1.1170x | 6.6950x |
| records | 34366589.63 | 20827336.37 | 1.6501x | 8.4153x |
| sort_bench | 7478126.47 | 5576355.37 | 1.3410x | 7.0596x |
| json_transform | 2526136.84 | 1599915.89 | 1.5789x | 1.7472x |
| string_build1 | 2211.86 | 1242.32 | 1.7804x | 7.5155x |
| templating | 1058323.00 | 565783.37 | 1.8705x | 3.0608x |
| empty_loop | 320.01 | 193.49 | 1.6539x | 3.4343x |
| prop_write | 1141.99 | 741.96 | 1.5392x | 9.5514x |
| func_closure_call | 2465.00 | 1771.85 | 1.3912x | 6.9359x |
| object_build | 5119.22 | 3767.59 | 1.3588x | 1.8696x |
| array_read | 877.60 | 695.26 | 1.2623x | 8.6492x |
| func_call | 1042.00 | 902.55 | 1.1545x | 2.9363x |
| prop_read | 1098.99 | 1023.26 | 1.0740x | 9.8161x |
| int_to_string | 1195.92 | 1479.97 | 0.8081x | 3.4446x |
| int_arith | 526.93 | 720.52 | 0.7313x | 2.4359x |
| float_arith | 608.27 | 1251.38 | 0.4861x | 2.3621x |

The landed FIG-7485 change installs resident text metadata directly.
Owned append already transfers the String and chunk buffer. Its new immutable
header no longer walks generic body members, cold chunks or flat collection
summaries, or replaces Facts already cleared when their slot was released.
Resident joins and concatenations use the same installer in both dialects.
Tokens, zero-list membership, residency, old operand summaries, work admission,
charge formulas and rollback remain unchanged. The original seven matched
charge and held-byte probes equalled that lane's baseline. Workloads, prices
and budgets are unchanged.

An archived FIG-7485 full optimized ablation removes only the formula-bound
refusal,
keeping exact-mode executor flags, formula evaluation, charge totals and memory
admission. Five 1/500-trip pairs preserve outputs and execution census; all
adapter resource totals agree, and the source is restored afterward. Removing
that check saves a net 11.11 instructions/append, with an observed pair-difference
envelope of 10.67 to 11.24. For templating it costs a net 310.84 more
instructions/trip, so it establishes no positive isolated contribution there.
These are effects of one check implementation, not an intrinsic lower bound.
The larger unexplained residual remains. FIG-7485 stays open under the rule
that an above-parity row needs a complete measured semantic explanation.

The completed lane proof covers 527 distinct tests, with 524 passes and
three existing conformance failures. Full conformance is not a passing gate.
The SQLite durability targets passed all 14 tests.

The archived FIG-7485 executable/source identities, counters, charge census,
parent counterfactuals and causal ablation are in
`/workspace/notes/lash/tasks/lanes/r33/pymonty-str2.evidence/`, especially
`after.json`, `verification.json`, `matrix-summary.json`, `charge-census.json`,
`integrated-core-verdict.json`, `integrated-parent-verdict.json` and
`formula-bound-assessment.json`. First-pass and collection-lane measurements
remain archived in the neighboring `pymonty-str.evidence` and
`pymonty-coll.evidence` directories.

Monty is the pinned 1.4.0 executable, SHA-256
`4479e7306fdf0c6a2a567aa139150ac47c75ab3c0138ec2c4efa010c382a87e0`.
Its public resource limits cover duration, allocator-backed memory, recursion,
suspensions and sleep; they do not expose Lash's deterministic formula budget.
The exact tag and published source identity are retained in
`monty-source-identity.json`. Full reference executable identities remain in
`after.json` and `baseline-binaries.sha256`.

The combined Lash executable SHA-256 is
`0285bf05a9c7bc723400aa7bf6a07fc97ea8cd26605fc571bb7a4198d5eaa42b`.

`--profile <directory>` performs a separate `perf record` run per selected lash
workload and dialect, writing perf data, perf-script stacks, and Inferno folded
stacks. It requires `perf` and `inferno-collapse-perf` on PATH. Profiles include
process setup and admission as well as execution; inspect named functions when
attributing VM cost. The clock receipt remains the ordinary unprofiled run.
For symbolized profiles, use the existing profiling platform when building:
`kiln build //crates/lash-perf:lash-perf__bin --target-platforms
//tools/buck2:profiling -c kiln.rust_profile=optimized --materializations final
--build-report .kiln/compare-profile-build.json`.
Use `--workload empty_loop,func_call` and `--dialect typescript` to focus a lane.
Profiling failures are explicit command failures with stderr retained.

For durable observations, build the existing `durable-substrate` and worker
executables optimized, then add `--durable-substrate <binary> --vm-worker
<worker> --dqts-hibernate <hibernate> --dqts-script <hibernate-sync.js>`. Lash
runs the existing `cell-0` geometry: ten parks, a resident engine and subprocess
worker, SQLite file/WAL with synchronous FULL/fsync. Its block-cycle statistics
measure park-to-next-park including dispatch, execution and commit. dquickts
execs a fresh process per slice, reading base+delta files and writing the delta,
with no database or fsync. Plain and resumed outputs must match. Slice counts,
sample times and all geometry are retained, with no equivalence ratio: these
measure different services and persistence guarantees. Omitted durable paths
produce no durable observation. Runtime stores/snapshots belong in disposable
run directories; retain receipts and logs as evidence.

Every Lash row also includes `execution_census` from the separate diagnostic
cell at `trips[0]`, after its first checkpoint. It counts physical statement
executions by owner, owner-to-owner handoffs (including returns), register
entries, and guard/slow exits by reason. Register entries count fresh virtual
calls; resuming an existing semantic frame adds no entry. Absent map entries
mean zero. Scalar
loop regions are separate from the continuous register program. A resumed
source continuation does not start another statement; speculative loop regions
count committed statements, while helper plans count their statement boundaries,
including loop-test statements and statements whose storage remains unopened.
The VM's canonical execution facts also feed the existing register-entry witness.

Counting enables no semantic observer and selects no different executor. The
ordinary timed population has no execution census; the diagnostic is outside
the scoped `--instructions` interval. Its instructions are fixed at the low
count and cancel when comparing high-count factors with paired whole-process
`perf stat -e instructions:u` samples. These counts describe
the diagnostic cell's actual execution, not sampled coverage or a claim that
all observation modes use one executor.

`--observe` enables work counts, live guest context, statement paths and site
counts and passive host-site callbacks in every timed and diagnostic Lash cell. The receipt records this choice
in geometry, so duration history cannot mix observed and ordinary populations.
Reference interpreters and the workload geometry are unchanged.
