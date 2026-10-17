# Kernel versus embedded interpreters

This instrument replaces the pre-heap ratio as the kernel speed yardstick
(FIG-6675, following the FIG-6318 ruling). It compares Lash TypeScript with
upstream QuickJS, and Lash Python with Pydantic Monty. Node and CPython provide
reference rows. It changes no interpreter, helper, charge formula or kernel
library definition.

The recorded table is **provisional: pre-FIG-6672**. FIG-6672 restores native
fast paths narrowed by FIG-6648; authoritative numbers and gap ranking belong
to the follow-up run on that base. This lane certifies the harness and equality
checks, and does not rank performance gaps.

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
and produces the provisional report table. It never installs workspace
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
`await finish(result)` boundary, using the existing instrument's scripted host.
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
| dict_record | 2,000 dynamic-key updates across 64 buckets |
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
program; this provisional report makes no ranking claim.

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
No before/after optimization claim or pre-heap baseline is made.

`receipt.json` holds every sample and result, executable/source digests and
release identities. `samples.jsonl` retains completed samples even if a later
one refuses. Each sample has `.stdout`, `.stderr`, `.perf.tsv` and `.time.txt`.
The report uses the arithmetic mean of the two samples and maximum RSS. Raw pair counts remain available, so a mean
cannot conceal a missing sample. There is no noise filter, automatic retry,
baseline re-pin or performance gate. Quiet-host wall time is unmeasured on this
shared host; follow-up optimizations should retain paired instruction evidence.
