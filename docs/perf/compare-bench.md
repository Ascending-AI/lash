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

The maintained instruction gate also runs `object_sort_200k` at 20,000 and
200,000 records with the same comparator source. It uses one warmup and one
measured pair for this large row; the other maintained rows retain five pairs.

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

The FIG-7528 full optimized receipt on 2026-10-28 measures compact caller
return metadata at source `c62bacfdd2cf009580e52611c0499e8b42f9d137`, against a matched
control at `671e8c038fb4b2e04427f5ada4eef74eca4e0fcc`. Each of the 85
engine/workload rows ran one low/high pair, one warmup and factor 20 on CPU 0,
with exact charge and memory accounting and an 8 GiB address-space cap.
All 68 scoped Lash and 102 reference PMU windows ran at 100%; all 34 Monty
outputs matched CPython. Workload source bytes, results, work units and reference
identities matched the control. All 34 Lash diagnostic execution censuses
also matched.

Every Lash row met the 1% regression guard. The maximum increase was 0.3433%
for TS templating. Every TS/QuickJS ratio met 10x; the maximum was 9.8240x
for prop_read. Values are incremental user instructions/op. Signed changes
compare each dialect with its matched Lash control; negative means fewer
instructions. Shared-host wall time is unclaimed.

| Python workload | Lash-Python instr/op | Monty instr/op | Lash/Monty | Lash-TS/QuickJS | Python change | TS change |
|---|---:|---:|---:|---:|---:|---:|
| object_sort | 15034.03 | 7769.55 | 1.9350x | 8.7599x | -0.080% | -0.142% |
| templating | 1062310.89 | 565617.95 | 1.8781x | 3.0834x | -0.165% | +0.343% |
| string_build1 | 2218.69 | 1242.39 | 1.7858x | 7.5359x | -0.012% | +0.035% |
| records | 33885364.21 | 20827465.05 | 1.6270x | 8.3704x | -0.002% | -0.160% |
| json_transform | 2520722.11 | 1597361.21 | 1.5781x | 1.7513x | -0.025% | +0.096% |
| func_closure_call | 2443.23 | 1771.86 | 1.3789x | 6.8829x | -0.367% | -0.366% |
| object_build | 5138.58 | 3767.61 | 1.3639x | 1.8867x | -0.062% | -0.063% |
| sort_bench | 7471956.16 | 5577374.11 | 1.3397x | 7.0571x | -0.014% | -0.006% |
| array_read | 883.10 | 695.25 | 1.2702x | 8.6537x | +0.001% | +0.000% |
| prop_write | 913.24 | 741.98 | 1.2308x | 9.4861x | -0.000% | -0.003% |
| func_call | 1039.23 | 902.55 | 1.1514x | 2.9257x | +0.000% | -0.002% |
| map_set_string | 6067.74 | 5407.98 | 1.1220x | 6.7027x | +0.001% | +0.005% |
| prop_read | 1105.24 | 1023.28 | 1.0801x | 9.8240x | +0.001% | -0.000% |
| int_to_string | 1197.56 | 1470.96 | 0.8141x | 3.4537x | +0.001% | +0.001% |
| int_arith | 487.88 | 720.52 | 0.6771x | 2.3011x | -0.011% | -0.005% |
| empty_loop | 116.94 | 193.39 | 0.6047x | 3.3600x | +0.000% | -0.003% |
| float_arith | 558.26 | 1251.32 | 0.4461x | 2.1846x | +0.022% | -0.018% |

A waiting caller now keeps a pending-call bit and uses its retained `pc - 1`
for the call instruction's return destination, charge suffix and source
projection. Source identity, logical frame costs and initialization are
unchanged. Register and slot clearing remain in place. Boxed windows,
entry-time clearing and conditional reuse of old parameter slots failed the
regression guard and were rejected.

Ordinary calls stopped moving: 1039.23 instructions/call, effectively equal
to the control. Their callee already takes the existing literal-return path.
Closure calls fell 0.367% and object_sort fell 0.080%. Weighting each of these
three rows by its own fixed comparison interval saves 2.979 million
instructions, below the study's 25.8 to 94.2 million forecast. No target row
reached Monty parity. FIG-7528 remains open for window reuse and the three
above-parity rows; no intrinsic floor above 1x is established. The study's
measured admission checks and sampled argument/operand paths remain the
next causes to investigate. Their contribution in this selected build is
still unisolated.

The receipt, every row's delta, rejected experiments and semantic proof are
in `/workspace/notes/lash/tasks/lanes/r33/pymattr3.evidence/`, particularly
`metadata.json`, `metadata-assessment.json`, `selected.json` and `report.md`.

FIG-7526 separately measured the operand/result pipeline against its own
ordinary optimized control at the same main `671e8c038fb4`. That control's
maximum TS/QuickJS ratio was 9.8257x. Its experiments and open findings are
retained here; the table above uses FIG-7528's qualified selected receipt.

Three operand/result experiments were rejected by the unchanged 1% all-row
guard. Fixed member operands, short heap/call windows and heap destinations
regressed seven Lash rows. Direct integer size-bit projection regressed
three. Compiled native-arity facts with short native argument windows
regressed eight. Every experiment kept TS/QuickJS at most 10x. No experimental
Rust or test change was retained, and no performance baseline or ceiling was
changed. Over one fixed high-minus-low interval for each of the 13 initially
above-parity Python rows, the experiments increased instructions by
18.294, 10.082 and 17.916 million respectively.
Operation units are not averaged.

No row newly reached parity in these experiments. The 13 above-parity Python
rows remain open without a measured intrinsic floor, and FIG-7526 remains
open. The next locations are the study's measured operand/destination,
member-result and native/call paths; the failed transformations establish no
minimum cost for them. Lookup changes still require integration of FIG-7520.
Raw per-row deltas, fixed-interval totals, next profile sites, source/binary
identities, counter receipts and restored patches are retained in
`/workspace/notes/lash/tasks/lanes/r33/pymattr1.evidence/report.md`.

FIG-7526's first rejected candidate's VM unit proof passed 218/218. Conformance
executed 142 cases with 137 passes and five identity/link/caption failures;
no conformance pass is claimed. Destination, integer-zero-size and native
arity mutations were rejected by the existing register/reference differential.
Their production restorations and probe limits are recorded in the lane
report. No parent or main tests were run for comparison. Earlier optimization
receipts and their qualified limits are retained below.

The implementation notes and receipts below describe the earlier
FIG-7484/7485/7486 changes.

Scalar assignment/test pairs now fuse during compilation. Verified recurrences
with one changing scalar and ordered copies execute checked kernels, exact fee
tables and the existing per-iteration headroom guard directly, avoiding the
private decoder and register rollback copy. Overflow replays the failing
iteration through the original source path. Recorded execution retains its
existing interpreter.

Literal short-text map stores reuse existing object/key/revision position
proofs. Eligibility comes from the verified source expression before charge
closing, independently of operand fusion. Dynamic keys retain the original
path. Equal-size scalar table overwrites and scalar cell writes avoid metadata
work when size and holding edges cannot change. Dirty marking and accounting
remain intact. No workload, helper-name whitelist, semantic or accounting
change was introduced; admission remains owned by FIG-7510's protocol.

Six core rows remain above parity without a complete intrinsic explanation:
prop_write, func_closure_call, object_build, array_read, func_call and prop_read.
FIG-7486 remains open. Baseline full optimized memory-off diagnostics account
for about 27 instructions/read and 22/store; the object-build census changes,
so its roughly 519-instruction delta is confounded. Charge-off changes
optimization selection and is excluded. These diagnostics do not establish an
intrinsic floor for the remaining gaps. No iteration result is used as an
intrinsic-cost explanation.

The earlier 392-source census at cffc67db78 versus main 931b05e2b0 found equal
charges and outcomes throughout. It predates the final cache-eligibility
correction; the final charge-bound, layout, held-byte and park/resume laws
passed in final-packages. No census or suite was repeated after the pool-budget
ruling. The separate, pre-existing executor memory-bound disagreement is
reported with its program and both held-byte figures in the lane report.

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

Each lane's recorded package proof executed 527 tests, with 524 passes:
Python 159/159, VM 216/216 plus linked dispatch 10/10, and conformance
139/142. SQLite passed 14/14 in each lane. FIG-7486's three conformance
assertion bodies exactly match the archived parent-main evidence at 13c6be0c63.
Full conformance is not a passing gate in these receipts. No parent or main
tests were rerun, and the completed proof is reused after clean rebases.

Current FIG-7486 evidence is in
`/workspace/notes/lash/tasks/lanes/r33/pymonty-core2.evidence/`: `final.json`,
`final-verification.json`, `final-assessment.json`, `proof-summary.json`,
`archived-main-failures.json`, `charge-census-verdict.json` and `report.md`.
Its measured executable SHA-256 is
`500eda779b1d96ce6f0a757b694a9e10ea50cf97a9620bad69892352911e2422`.

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

The FIG-7484 measured Lash executable SHA-256 is
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
