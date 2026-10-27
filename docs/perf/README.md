# Performance instruments

Run from an isolated Kiln fork after `. ./env.sh`. Profiling scripts build
through Kiln, materialize final outputs, and resolve executables from the
output report. `--no-build --build-report <report>` consumes that report;
there is no Cargo target-directory fallback. Runtime and stack scripts also
accept an explicit prebuilt `--binary`. Keep reports and logs under the
fork's `.kiln/<ticket>/` directory, and run executables in place.

These reduced runs establish instrument functionality on the current source.
They do not establish timings, a noise floor, or a regression baseline. Use a
quiet host and the full workload for performance comparisons. The
committed historical baseline and substrate-comparison documents retain past
results; their old launchers have been retired.

## Writing a perf instrument

1. Declare every number's quantity, unit, process or window, and statistic.
   Label configured values, endpoint samples and upper bounds explicitly in
   both the receipt and its text summary.
2. Declare diagnostic output as noncertifying. A certifying caller must consume
   completeness, qualification and every selected verdict before succeeding.
3. Validate experiment identity before comparing: workload, geometry, build,
   compiler, allocator, backend, durability, host and sample meaning. Refuse
   mismatches and missing required metrics.
4. Capacity and tail claims need scheduled arrivals, finite admission capacity,
   unfinished/error counts, and offered and achieved rates. Label closed-loop
   populations as service diagnostics.
5. Prove reach with an actual producer path, matching authority and features,
   an observer/consumer and a command receipt. Definitions and synthetic
   adapter tests establish capability alone.

## Developer runbooks

- [Interpreter comparison](compare-bench.md): matched kernel and reference-engine workloads.
- [History component probe](history-probe.md): held-frame, checkpoint work and ownership growth.

## Counted regression gates

The scheduled main run executes the replay counted gate through
`kiln test //:main_tests`. The PMU and startup commands execute locally after
that suite, because remote workers need not expose a PMU. Landing runs Clippy;
it does not execute these performance workloads.

| Regression | Measurement and gate | Execution |
|---|---|---|
| Extra SQL write per turn | Replay `sql_writes`, rows changed and bound bytes | Main counted suite |
| Extra worker allocation per guest read | Harness worker allocator deltas in progress receipts | Main counted suite |
| Extra CPU instructions | `instructions:u` per operation, host threads and workers separately; 1% above previous successful main baseline | `just perf-gate-instructions` locally |
| Extra startup sleep | Median requested sleep nanoseconds and sleep calls over 20 actual helper spawns, traced through Ready | `just perf-gate-startup` locally |
| Repeated provider metadata per tool body | Snapshot, manifest, link-payload and body-entry counts in the replay fixture provider | Main counted suite |
| Retained bytes per facade handle | Net live Rust bytes since session handles opened, sampled after the final turn with handles still held | Main counted suite |

The counted gate fixes its Tokio runtime at eight workers on local and hermetic executors.
The private single-node fixture uses after-commit wake hints and schedules periodic
heartbeat, reap, liveness/claim polling and retirement beyond the measured run;
those maintenance costs are outside this foreground-work population.
It checks the median of five independent per-turn medians. It retains
each raw population beside the aggregate receipt. Maxima and depth slopes
retain the worst observation. Replay ceilings use the maximum of five same-commit runs plus their observed
max-minus-min absolute noise. Exact metrics get zero slack. Every measured
maximum, noise and resulting ceiling is in
`crates/lash-perf/workloads/replay/baseline.json`. `just perf-replay-bless`
collects all five receipts in independent processes and refuses to raise an existing ceiling. The
committed depth-slope and independent hard ceilings still bind. The final
five-run calibration after FIG-7446 measured 87 chat-short writes/turn and 97 compacting-chat
writes/turn, both with zero noise. Full maxima, noise and ceilings are retained
in the baseline for all 96 medians, including entries per actual tool body.
The fixture stores use fixed-length private canonical paths during measurement:
SQLite embeds its canonical path in publication domains, so artifact-directory
length must not change SQL bytes or allocator work. Closed stores are copied
to the requested artifact directory after measurement. These are
finite same-commit observations, not a population-wide noise bound. SQL reads
include asynchronous background work: existing observer origin tags do not
provide a complete foreground/maintenance classification across all runtime
tasks, so a one-read regression is not certified. Their measured aggregate
ceilings remain enforced.

Probe (e) is unproven and stale: its patch targets `SurfaceTools::execute`, which ordinary facade replay dispatch no longer runs.

The retained-byte window includes host caches, the store and the result ledger
created while those actual facade handles are held. It measures the final
live level, not allocation traffic, direct session state, or RSS. The receipt
contains that metric only on the final round. It is divided by the retained
session population, like the other replay counts.

PMU samples retain raw count, enabled time and running time. Missing events,
empty populations, nonpositive instruction slopes, or less than 99% running
time refuse the gate. Host events open before the runtime and inherit threads
only; worker events are opened in the credential-free worker before confinement.
The gate never substitutes guest charge or scaled wall durations. Successful
clean main runs atomically advance `/workspace/kiln/lash/perf-gates/` baselines;
a failure cannot replace its reference. A committed measured baseline bootstraps
the first run. Workload source, geometry, host, compiler, build and allocator
must match. A baseline from another history is refused. Kernel fits use 1,000
to 100,000 trips for the small loops; the large object workloads retain their
10,000 to 100,000 object population. The CPU reference contains all 40 rows from the first complete population,
with five paired trip samples per kernel row. Its independent reverted control
passed all 40 rows; its largest upward change was 0.845%. These finite
observations do not establish a population-wide noise bound. Count calibration
uses five independent processes, and startup calibration uses five populations
of 20 Ready handshakes.

Startup wall medians are retained as diagnostics. The startup verdict counts
requested blocking syscalls, using `strace -f` on the exact helper PIDs in the
20 Ready receipts. Shared-host scheduling cannot turn a requested 50 ms sleep
into a pass. This catches added sleeps; it does not claim to gate page faults,
filesystem wait, or all other causes of startup latency. Startup CPU work is
outside the steady replay PMU population.

For the external `tests-main/run.py`, run these commands from its main fork
with `env.sh` sourced, and propagate either exit code as a red step:

```sh
just perf-gate-instructions
just perf-gate-startup
```

`just perf-history-record` captures the instruction gate's complete kernel and
replay populations once on a clean commit landed on main. It fetches main and
requires HEAD to be an ancestor of origin/main, so a newer landing during the
gates does not refuse the sample. It appends qualified
instructions per operation, SQL statements/writes, and host/worker allocation
counts to `.benchmarks/perf-guard/runtime-duration-history-full.jsonl`, using the existing JSONL
history store and level-shift detector. Set `LASH_PERF_HISTORY_FILE` to retain
that store outside a disposable checkout. The existing detector reports a
shift after five consecutive observations more than 50% above the preceding
median; its retained evidence and reviewed acknowledgements also apply to
counted series. The 1% CPU gate remains a separate immediate check. No duration
fields, latency samples, or producer timing tables are retained by this recipe.
Producer hashes remain provenance; changed workload geometry, compiler, build,
allocator, backend or host select another comparison cohort.

`perfreport --diff` requires matching complete experiment identities and sample
geometry. `cellpath` refuses a failed or missing cell even if its synthetic
model subsequently answers. `scripts/perf_guard_budgets.json` retains enforced
allocation limits and required phase inventory; unused wall ceilings and the
dead frame-curve configuration have been removed.

## Instruments

- [async-scheduling-queues](instruments/async-scheduling-queues.md): how operations move through async task scheduling and the stores' physical queues.
- [binary-size](instruments/binary-size.md): what an optimized binary's size consists of, by section, symbol and attributed crate.
- [boundary-tails](instruments/boundary-tails.md): what the retained per-operation boundary latencies look like at the tail.
- [compiler-self-profile](instruments/compiler-self-profile.md): where rustc spends its time compiling a chosen crate.
- [cpu-profiles](instruments/cpu-profiles.md): where a workload spends CPU and where its threads block off-CPU.
- [durable-substrate-waits](instruments/durable-substrate-waits.md): how long direct durable-substrate process registration and waits take.
- [durable-transaction-telemetry](instruments/durable-transaction-telemetry.md): what per-statement elapsed time inside one durable PostgreSQL commit attempt shows.
- [kernel-gate6](instruments/kernel-gate6.md): strict raw kernel receipts, recorded baseline comparison and advisory history.
- [duration-history](instruments/duration-history.md): how durations compare against retained history and what operation tails show.
- [fixture-graph-curves](instruments/fixture-graph-curves.md): whether the named runtime fixtures reach real product paths and how the counted graph curves scale.
- [flight-recorder](instruments/flight-recorder.md): what bounded recent context a snapshot captures on a slow or failed operation.
- [heap-over-time](instruments/heap-over-time.md): which allocation stacks retain bytes over time beside the RSS series.
- [heap-profiles](instruments/heap-profiles.md): what DHAT attributes to lifecycle, child and worker heap windows.
- [independent-boundaries](instruments/independent-boundaries.md): what each independent 1.0 boundary case measures.
- [kernel-guest-profiles](instruments/kernel-guest-profiles.md): which guest functions, helpers and natives do the kernel's work and hold its time, by exact counters and a sampled guest context.
- [load-models](instruments/load-models.md): how a receipt's operations were generated and which model certifies tails and capacity.
- [observation-workloads](instruments/observation-workloads.md): what crosses the store boundary under process, session, trace-sink and overlay observation workloads.
- [offered-load](instruments/offered-load.md): how the scheduled-arrival generator measures offered-load latency and the saturation knee.
- [otel-export](instruments/otel-export.md): how a host exports lash spans and metrics through its own OTel providers.
- [pg-statements](instruments/pg-statements.md): what the PostgreSQL server executed for one facade workload.
- [replay-workloads](instruments/replay-workloads.md): what standard replayed sessions cost per turn, how that grows with history, and which counted budgets every merge holds them to.
- [runtime-profiles](instruments/runtime-profiles.md): what runtime, stack and allocation profiles of a scenario record.
- [send-to-completion-latency](instruments/send-to-completion-latency.md): what send-to-completion latency the diagnostic cases measure.
- [sql-operation-windows](instruments/sql-operation-windows.md): what physical SQL work one operation window performed.
- [startup-provider-http](instruments/startup-provider-http.md): what startup phases and loopback provider HTTP exchanges cost.
- [trace-records-spans-metrics](instruments/trace-records-spans-metrics.md): which records, spans and counters the instrumentation contract defines and who produces them.

## Recorded measurements

- [Persistent-heap statement cost, 2026-10-17](kernel-statement-cost-2026-10-17.md): Sam's accepted FIG-6318 verdict, paired instruction ratios, TypeScript member durations, and routing to FIG-6675, FIG-3843 and `triage-7c5c`.

## What was this turn waiting on?

| Level | Tool and evidence | Scope |
|---|---|---|
| Async task | `just perf-tasks <fresh-private-directory>` writes `console.json`, spawn sites and busy/idle/scheduled elapsed times, plus dropped events | Opt-in host executable and Tokio tracing build. Publish every 1 s, retain completed tasks 60 s. No library installs a subscriber. |
| PostgreSQL | `boundary --case pg-facade --pg-statements --pg-waits` writes `pg-waits.json` | Private PG18 diagnostic image, 10 ms samples joined to workload PID/start and query IDs. Reset, reuse and missing coverage refuse attribution. |
| Kernel/thread | [`profile_runtime.py --off-cpu`](instruments/cpu-profiles.md#off-cpu-capture) | Sleeping OS stacks with source lines. Several threads overlap; async future idle time is a separate quantity. |
| Long boundary tail | [`receipt-tail`](instruments/boundary-tails.md) | First-100,000 exact ledger plus full-population HDR estimates, exact max and ten slowest operation IDs. Quantiles in overflow are censored `>60 s`. |

Console uses `console-subscriber 0.5.0` and `console-api 0.9.0`. The optional
`console` feature and `-c kiln.console=true` enable Tokio tracing and
`tokio_unstable`. Ordinary builds leave both disabled. Install `tokio-console` on PATH before invoking the recipe. The recipe attaches
the interactive client through a recorded pseudo-terminal. The receipt client
speaks the Tokio Console API over a socket in a mode-0700 directory. For an
interactive view, attach `tokio-console file://localhost<directory>/tasks.sock`
while the replay runs. The receipt includes task/resource/async-operation event
drops. Poll busy elapsed time is not CPU time; this view cannot reconstruct a
suspended future's stack or see VM worker processes.
Console also counts task-span destructor entries as polls. A cancelled task's
`scheduled_ns` can include its preceding sleep; it is not sufficient evidence
of run-queue starvation. See [task-span timing limits](instruments/async-scheduling-queues.md#tokio-console-task-spans)
before interpreting scheduled totals or `completed`.

`perf-tasks` measures the same `concurrent-16` replay with tracing off and on,
using the optimized symbolized frame-pointer platform for both configurations,
using paired `perf stat -e instructions:u`. The default is two rounds across
16 sessions. Longer recordings can exceed Console's 4 MiB update/snapshot limit;
keep the population bounded to preserve 60-second completed-task retention. Its cost includes setup, server,
embedded receipt client and two final publication intervals. The external TUI
is outside the measured process tree. It does not claim quiet-host
wall-time overhead. Use the two workload receipts to check useful work.

PostgreSQL smoke, with a materialized `lash-perf` binary in `$B`:

```sh
kiln gate lash r33-waits -- scripts/ci/with-service.sh --durable-postgres --pg-waits pg -- \
  bash -c 'ulimit -v 8388608; "$1" boundary --case pg-facade --operations 8 --callers 1 \
    --postgres-url "$LASH_POSTGRES_DATABASE_URL" --pg-statements --pg-waits \
    --store-dir "$2/store" --out "$2/receipt.json"' _ "$B" "$E"
```

Only this diagnostic service image installs the checksum-pinned
`pg_wait_sampling` revision and sampling/reset metadata patch. It never changes
production servers. Counts times 10 ms estimate wait occupancy, not exact query
latency. The service disables no-wait samples rather than calling them CPU.
Client waits can retain the prior query ID. Eight sends prove the
producer works; they cannot establish rare tails. A changed backend population,
reset generation, absent sampling progress, uncovered endpoint or sampling gap
over 20 ms refuses attribution and retains its reason.
