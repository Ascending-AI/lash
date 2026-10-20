# Kernel versus embedded interpreters

This instrument replaces the pre-heap ratio as the kernel speed yardstick
(FIG-6675, following the FIG-6318 ruling). It compares Lash TypeScript with
upstream QuickJS, and Lash Python with Pydantic Monty. Node and CPython provide
reference rows. It changes no interpreter, helper, charge formula or kernel
library definition.

The [recorded comparison](../kernel-vs-embedded.md) measures the morning baseline
and the cutover head on the same basis (FIG-6719). Cold counts and differential
warm estimates are separate; neither is a durable-engine or park measurement.

From a Kiln fork, run:

```sh
. ./env.sh
export KILN_RE_PRIORITY=100
(ulimit -v 8388608; /usr/bin/time -v python3 scripts/perf/kernel/embedded.py --fetch-engines \
  --out .kiln/FIG-6675/comparison \
  --report docs/perf/kernel-vs-embedded.md)
```

Use a fresh output directory on every invocation. This one command resolves
checksum-pinned release executables, builds `kernel_perf` through
`kiln build --config=optimized`, runs the entire corpus, checks every result,
and produces a cold report table. It never installs workspace
dependencies or invokes Cargo. Downloads require the explicit `--fetch-engines`
opt-in; subsequent runs can omit it. Third-party executables and archives live
in ignored `target/kernel-embedded-tools/`, outside evidence and source control.
The remaining requirements are Linux x86_64, Python 3, Node with direct `.ts`
execution (the recorded run uses 25.2.1), GNU time, and permission to read
`perf stat -e instructions:u`. The pinned Monty binary requires glibc 2.28+.
A denied or multiplexed instruction counter fails the instrument.

An already built artifact can be selected with `--kernel PATH`. `--program NAME`
selects an exact diagnostic subset; it does not certify the full corpus.
Offline rendering does not execute engines:

```sh
python3 scripts/perf/kernel/embedded.py --out .kiln/FIG-6675/comparison \
  --render .kiln/FIG-6675/comparison/receipt.json \
  --report docs/perf/kernel-vs-embedded.md
```

## Differential warm execution

Add `--differential-warm` with a fresh output directory to measure repeated
executions in each cold process. This is the FIG-6719 measurement method:

```sh
(ulimit -v 8388608; /usr/bin/time -v python3 scripts/perf/kernel/embedded.py \
  --differential-warm --out .kiln/FIG-6719/current-warm \
  --report .kiln/FIG-6719/current-warm.md)
```

The adapter wraps the unchanged body in a zero-argument function returning
`result`, then calls it N or 2N times, printing only the final result. Each
engine parses and prepares that source once per process; Lash shares one
`PreparedLibrary` and document across all calls. N is 2 for sum1m, branch1m and
fib25, and 100 for the shorter programs. Both counts have the same two reversed
engine-order pairs, resource cap, load sampling, counter checks and result
checks as cold runs. The complete warm receipt contains 312 processes. All
results must also agree across N and 2N; the committed comparison checks them
against the original cold results.

For each pair, the incremental cost of N body executions is
`instructions(2N) - instructions(N)`; the table divides that by N to report one
body execution. Ratios divide the two-pair mean Lash delta by the corresponding
peer mean delta. Fixed process startup, parsing, library preparation, the final
finish/print and teardown cancel to first order. No nonpositive delta is
accepted as a run cost. A Node reference pair with a nonpositive delta is
reported as unavailable with its raw signed difference; adaptive JIT and
startup variance can defeat that estimate. Such a reference refusal does not
block the primary peer ratios. Other nonpositive deltas still refuse the
report. The receipts retain both complete counts rather than hiding the
subtraction. No sample is retried.

This is an estimate of warm execution, not an acknowledged run-only hardware
window. It includes the repetition loop, function calls, result replacement,
and their allocations and cleanup. Wrapping also makes the body's bindings
function-local instead of top-level session bindings, and nested function
definitions execute on each call. Compilation of the different repetition
literal and any nonlinear GC/allocator effects need not cancel exactly.
Node's adaptive JIT behavior is an additional reason it remains a reference.
The unchanged cold measurements preserve the original program geometry.
RSS is the maximum of the N/2N whole-process peaks, not a phase-specific peak.
Lash's differential `run_ns` uses the same subtraction; lower, library compile
and document compile/start timings remain available for each input process.

## Release identities

The opt-in fetcher verifies the complete archive SHA-256 before extracting only
the named executable. These are the vendors' published optimized Linux builds;
they are not locally rebuilt with Lash's compiler flags. Exact executable
hashes, URLs, archive hashes, actual Node/CPython versions, Lash's compiler and
profile, host CPU/kernel and source digests are retained in `receipt.json`.

| Engine | Distribution | Archive SHA-256 |
|---|---|---|
| QuickJS | [Upstream 2025-09-13 Linux x86_64 release](https://bellard.org/quickjs/binary_releases/) | `648dc418356b7fa627f78bdf43b12a1cfd94bc220fc36520e1320e36c9328d10` |
| Monty | [pydantic-monty-runtime 1.1.0 manylinux_2_28_x86_64 wheel](https://pypi.org/project/pydantic-monty-runtime/1.1.0/) | `7cee4303924ad44a858ec3efdde4123cadbf032cb23006a2d260c73d68748d60` |

Monty's [CLI](https://pydantic.dev/docs/monty/reference/cli/) executes Python
files directly. This avoids counting a CPython binding or a Python-side pool
as interpreter execution. QuickJS executes the JavaScript subset of the plain
TS source. Both adapters use file execution, without type checking or JIT warmup.

## Equal work

The maintained corpus is the paired `.ts` and `.py` files in
`scripts/perf/kernel/embedded/`. Each body leaves `result`. The CLI adapter adds
`console.log(JSON.stringify(result))` or `print(result)`. Lash adds one
`await perf.finish(result)` boundary, using the existing instrument's scripted host.
It converts the resulting Datum to the same JSON value domain after the run
window. That boundary's fixed cost remains in Lash's run and cold counts.

| Program | Work in every engine |
|---|---|
| sum1m | Explicit while loop, add integers 0 through 999,999 |
| branch1m | Same loop count, add even indexes and subtract odd indexes |
| fib25 | Naive recursive Fibonacci, `fib(25)` |
| chain_mutation | 150 nested row replacements and counter increments, same shape as gate 6 |
| allocation_churn | 400 records with pair and label, keep every 40th, same shape as gate 6 |
| large_data | 512 values into 16 buckets, even/odd lists and joined summary, gate 6 geometry |
| string_build_join | 500 labels, repeated string concatenation and a materialized list join |
| dict_record | 2,000 dynamic-key updates across 64 buckets, keys built by `String(n % 64)` / `str` |
| dict_record_plus | dict_record with keys built by `"key_" + (n % 64)` (TS) / `"key_" + str(n % 64)` |
| dict_record_template | dict_record with keys built by a template / f-string |
| list_map_filter | Materialize 800 inputs, 800 doubled values, then multiples of three |
| workflow_iteration | `examples/workflows/iteration-untyped`, 128 strings through pure echo |
| workflow_early_return | `examples/workflows/early-return`, 128 calls spanning both branches |

The workflow bodies retain their loops, branches, calls and results, with
`corpus.echo` replaced by the same pure identity function and `async`/`await`
removed in both languages. The entry wrappers supply equal deterministic input.
They measure workflow computation, without I/O, promise scheduling, effects,
parking or durable engine latency. Python list comprehensions and JS map/filter
are the natural supported spellings; both materialize the same intermediates.
Python dictionary access and JS record access retain their language semantics.
No engine substitutes a precomputed answer or removes requested work.

All results are complete JSON-domain values, including full strings, lists and
records. Recursive canonicalization sorts record keys and normalizes exact
integral floats; nonintegral/nonfinite numbers, non-text keys and integers
outside JS's exact range fail. SHA-256 covers compact UTF-8 canonical JSON.
Every sample must match every other engine and pair for that program before a
complete receipt or report is written. stdout, stderr and partial samples remain
available on failure. A mismatch is never skipped or accepted as a timing row.

## Windows and evidence

Two pairs run in sequence for each program: Lash TS then QuickJS, Lash Python
then Monty; the second pair reverses each order. Node and CPython reference runs
follow each pair, also in reversed order. Each sample is a new process, with
one execution and no warmups. Instructions count the complete cold invocation,
including loader, registry, compilation, output, teardown and GNU time wrapper.
That is a reproducible cold workload metric, not steady-state execution speed.
A large fixed registry cost can explain a large cold-process gap for a small
program. Compare these counts separately from the differential warm estimates.

Wall time measures the complete process; cold start to first result measures
spawn to the first complete stdout line. The line can be delayed by CLI stdout
buffering. This is a cold process on a warm host filesystem cache, not a cold
machine. GNU time records the workload's peak RSS and resources. `load1` is
sampled immediately before each process. Every measured process and its
wrappers inherit a hard 8 GiB `RLIMIT_AS`; an exceeded cap fails, as does the
30-minute per-process timeout. Node runs with a 512 MiB old-space ceiling and
Wasm trap reservations disabled so it can start under the same address-space
cap; this small corpus does not approach that heap ceiling.

Lash reports registry assembly (including argument parsing and source reads),
lowering, library compilation, document
compilation plus cell start, and run separately. The public API performs the
last compilation lazily inside start; the instrument does not claim to separate
it from cell initialization. Both dialect helper sets are present, matching the
existing kernel instrument. Run includes the finish host boundary. Its allocator
is `system+stats_alloc`; run allocation/reallocation calls and metered charge are
recorded without the tracing/work observers. Charge is not a VM statement count.
The FIG-6719 report compares the two named cutover revisions, with source and
artifact identities retained separately. It makes no pre-heap comparison.

`receipt.json` holds every sample and result, executable/source digests and
release identities. `samples.jsonl` retains completed samples even if a later
one refuses. Each sample has `.stdout`, `.stderr`, `.perf.tsv` and `.time.txt`.
The report uses the arithmetic mean of the two samples and maximum RSS. Raw pair counts remain available, so a mean
cannot conceal a missing sample. There is no noise filter, automatic retry,
baseline re-pin or performance gate. Quiet-host wall time is unmeasured on this
shared host; follow-up optimizations should retain paired instruction evidence.
