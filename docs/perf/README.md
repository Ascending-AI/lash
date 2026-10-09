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

## Instruments

- [async-scheduling-queues](instruments/async-scheduling-queues.md): how operations move through async task scheduling and the stores' physical queues.
- [binary-size](instruments/binary-size.md): what an optimized binary's size consists of, by section, symbol and attributed crate.
- [boundary-tails](instruments/boundary-tails.md): what the retained per-operation boundary latencies look like at the tail.
- [compiler-self-profile](instruments/compiler-self-profile.md): where rustc spends its time compiling a chosen crate.
- [cpu-profiles](instruments/cpu-profiles.md): where a workload spends CPU and where its threads block off-CPU.
- [durable-substrate-waits](instruments/durable-substrate-waits.md): how long direct durable-substrate process registration and waits take.
- [durable-transaction-telemetry](instruments/durable-transaction-telemetry.md): what per-statement elapsed time inside one durable PostgreSQL commit attempt shows.
- [duration-history](instruments/duration-history.md): how durations compare against retained history and what operation tails show.
- [fixture-graph-curves](instruments/fixture-graph-curves.md): whether the named runtime fixtures reach real product paths and how the counted graph curves scale.
- [flight-recorder](instruments/flight-recorder.md): what bounded recent context a snapshot captures on a slow or failed operation.
- [heap-over-time](instruments/heap-over-time.md): which allocation stacks retain bytes over time beside the RSS series.
- [heap-profiles](instruments/heap-profiles.md): what DHAT attributes to lifecycle, child and worker heap windows.
- [independent-boundaries](instruments/independent-boundaries.md): what each independent 1.0 boundary case measures.
- [lash-vm](instruments/lash-vm.md): what the LashVm perf, profile and function-perf measurements cover.
- [load-models](instruments/load-models.md): how a receipt's operations were generated and which model certifies tails and capacity.
- [observation-workloads](instruments/observation-workloads.md): what crosses the store boundary under process, session, trace-sink and overlay observation workloads.
- [offered-load](instruments/offered-load.md): how the scheduled-arrival generator measures offered-load latency and the saturation knee.
- [otel-export](instruments/otel-export.md): how a host exports lash spans and metrics through its own OTel providers.
- [pg-statements](instruments/pg-statements.md): what the PostgreSQL server executed for one facade workload.
- [runtime-profiles](instruments/runtime-profiles.md): what runtime, stack and allocation profiles of a scenario record.
- [send-to-completion-latency](instruments/send-to-completion-latency.md): what send-to-completion latency the diagnostic cases measure.
- [sql-operation-windows](instruments/sql-operation-windows.md): what physical SQL work one operation window performed.
- [startup-provider-http](instruments/startup-provider-http.md): what startup phases and loopback provider HTTP exchanges cost.
- [trace-records-spans-metrics](instruments/trace-records-spans-metrics.md): which records, spans and counters the instrumentation contract defines and who produces them.
- [vm-worker-matrix](instruments/vm-worker-matrix.md): what the optimized VM worker exchange matrix certifies.
