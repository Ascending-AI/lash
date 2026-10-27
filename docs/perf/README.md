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
