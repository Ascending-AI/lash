# Interpreter comparison

`lash-perf compare` is a developer benchmark, outside the merge path. It runs
fresh TypeScript and Python kernel cells using `built_embedding`'s production
worker natives and the released helper library, just as Test262 does. Guest
memory and exact charge metering stay on. It does not start the durable engine
for compute measurements. The testing-only
`lash_vm_worker::core_support::compare` adapter owns lowering, linking and
fresh machines; the harness receives opaque prepared cells and measurements.
Production hosts continue to submit runs through the worker service.

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
comprehensions and sum for filter/map/reduce; its JSON workload uses a source
codec restricted to this fixture's unescaped ASCII flat records, because the
dialect has no stdlib imports. CPython executes that identical codec.

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

## Current comparison

The full optimized run on 2026-10-27 completed all **17/17 Python workloads**
on Monty 1.4.0. All **170/170 measured Monty outputs exactly equal CPython's**,
including the floating-point row. There were **85 engine/workload rows and
850 measured samples**, with no unsupported features, missing rows or checksum
errors. Lash uses fewer instructions than Monty for integer arithmetic, and
more for the other 16 rows. The largest ratios are JSON transformation at
14.67× and keyed object sorting at 11.68×. No interpreter changes were made.

This is one full optimized compare invocation at implementation commit
`565d2761a7c8f5b0e9adc52beac2a58641070a93`, based on main `46b88ac7dbccd3278db4fed77b06112fd62caa97`.
Main at run start was `7064395d3a437e377b0c5c541af97463d570e40b`; its intervening prompt/read-race
changes did not change the benchmark or interpreter sources. Every engine
received the checked-in workload, including the restricted JSON codec then on
main. The stdlib JSON workload from the concurrent lane was not yet on main.

Five measured pairs, one fixed low-count warmup, factor 20, CPU 0, observation
off, exact charge and memory metering on. The 340 scoped Lash windows and 510
reference processes all qualified, with 100% PMU running time. Peak whole-run
RSS was 482,508 KiB under the 8 GiB address-space cap. Shared-host wall time is
not a latency claim. Values below are median incremental instructions/op;
ratios above one mean Lash executed more instructions. Different Python/TS
work units, especially sorting, keep their own denominators.

| Python workload | Lash-Python instr/op | Monty instr/op | Lash/Monty | Lash-TS/QuickJS | Likely cause from source |
|---|---:|---:|---:|---:|---|
| json_transform | 102940835.89 | 7015212.26 | 14.67× | 2.14× | The restricted codec repeatedly splits/replaces text, calls guest functions and accesses dictionaries. |
| object_sort | 90770.03 | 7769.08 | 11.68× | 8.87× | Key extraction calls a Python lambda and reads each record before the native key sort; the secant is nonlinear. |
| map_set_string | 45581.25 | 5408.45 | 8.43× | 8.01× | Each batch builds 1,000 text keys twice, stores dictionary entries and checks membership. |
| records | 174035856.11 | 20827344.47 | 8.36× | 8.51× | Two materialized comprehensions, dictionary reads and helper-based summation traverse 10,000 records. |
| templating | 3461959.00 | 565692.79 | 6.12× | 7.34× | Two f-string substitutions per line use Python formatting helpers before text concatenation. |
| sort_bench | 21633352.16 | 5576494.58 | 3.88× | 22.98× | Text conversion, list population and order-checking surround the native sort. |
| array_read | 2194.81 | 695.25 | 3.16× | 8.64× | Ten indexed reads per trip retain typed access guards and VM operand handling. |
| prop_read | 2338.00 | 1023.24 | 2.28× | 9.89× | Four dictionary reads per trip use the proven dictionary helper, including its membership check. |
| string_build1 | 2483.33 | 1242.50 | 2.00× | 8.41× | The inner loop repeatedly concatenates and replaces text with Python add semantics. |
| float_arith | 2431.69 | 1251.47 | 1.94× | 2.36× | Multiply and float-add operations retain Python numeric dispatch alongside loop control. |
| prop_write | 1409.00 | 742.00 | 1.90× | 9.63× | Four resident map stores per trip retain VM/store bookkeeping around each update. |
| empty_loop | 325.99 | 193.53 | 1.68× | 3.42× | Only range/loop control and VM metering remain; their individual shares are unmeasured. |
| object_build | 5897.86 | 3768.07 | 1.57× | 1.90× | Record/list allocation, integer remainder and list append occur once per object. |
| func_closure_call | 2472.99 | 1771.85 | 1.40× | 6.98× | Four calls per trip mutate a captured binding, adding closure access to call/return work. |
| func_call | 1182.99 | 902.59 | 1.31× | 2.95× | Four virtual calls per trip return a constant, so call/return and accumulation are the main work. |
| int_to_string | 1770.70 | 1470.97 | 1.20× | 3.43× | Three integer conversions per trip use the Python string helper and native number formatting. |
| int_arith | 534.99 | 720.53 | 0.74× | 2.43× | The scalar integer loop reaches numeric natives; its measured instruction count is below Monty. |

The cause column is a source-based hypothesis, not a profile or causal
ablation. Typed indexing is selected in
[`lower/subscripts.rs`](../../crates/lash-dialect-python/src/lower/subscripts.rs).
Its Python helper definitions, numeric/string operations and mutation rules
are in [`core.kernel`](../../crates/lash-ext-py-helpers/src/helpers/core.kernel).
Collection methods live in
[`methods.kernel`](../../crates/lash-ext-py-helpers/src/helpers/methods.kernel);
key extraction and summation in
[`builtins.kernel`](../../crates/lash-ext-py-helpers/src/helpers/builtins.kernel);
f-string formatting in
[`format.kernel`](../../crates/lash-ext-py-helpers/src/helpers/format.kernel).
VM fast paths can bypass helper execution, so these sources do not establish
how much of a row's instructions each helper consumed.

The reference identities are Monty executable
`4479e7306fdf0c6a2a567aa139150ac47c75ab3c0138ec2c4efa010c382a87e0`,
QuickJS source `04be246001599f5995fa2f2d8c91a0f198d3f34c`, executable
`ec68673b92708e38971b58682bae94ca3775e106358c82f6fcd55526b8fd3ec4`,
and CPython 3.12.3. The receipt retains each executable/source hash.
Raw evidence is `/workspace/notes/lash/tasks/lanes/r33/monty.evidence/`,
including `compare.json`, its Markdown output, 510 reference perf files,
source wrappers/stdout/stderr, `verification.json`, result-population SHA-256s,
release identities, the full optimized build report, and outer perf/time logs.

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
