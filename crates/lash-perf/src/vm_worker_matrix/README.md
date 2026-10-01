# Integrated VM worker measurements

FIG-4162 measures the shipped worker pool, framed protocol, owned VM and reset
lifecycle against a developer-only in-parent reference on the same machine.
The reference is never an SDK execution mode. No provider, store, journal or
real tool latency is included. Effect answers come from a fixed echo host;
results and effect counts must agree before a timing can be recorded.

Source the fork environment. Build and run an optimized executable through kiln:

```sh
. ./env.sh
report=$(mktemp)
kiln build --config=optimized --materializations final --build-report "$report" \
  //crates/lash-perf:vm-worker-matrix__bin
binary=$(python3 tools/buck2/outputs.py --report "$report" \
  --label //crates/lash-perf:vm-worker-matrix__bin --single)
kiln gate lash FORK -- "$binary" --verify
kiln gate lash FORK -- "$binary" --out .benchmarks/vm-worker
rm -f "$report"
```

`--verify` drives every workload on both sides and checks the profiler's exact
report, results, effect counts, segment resumption, reset reuse across owners,
reservation across 256 growing lease IDs and a genuinely queued checkout.
Run it twenty times and inspect each log. It has no timing population. The
measurement mode requires 10,000 observations per warm workload and 200 new
worker processes per cold workload. It does not accept smaller samples.

`--exchanges SAMPLES --out DIRECTORY` runs only the warm effect-exchange populations, with
`SAMPLES` cases each, and writes raw CSV, summary JSON, budget JSON and a Markdown report. It is the paired before/after comparison for a change to
the exchange path (FIG-4433): build the two revisions' executables, alternate
them in one run, and compare the medians of at least five runs a side. The
timings remain a shared-host population; the acceptance distribution is the full
matrix above on a quiet host (FIG-4172).

The translated profiler preserves its ten original feeds. Fresh-state churn
uses one new owner per observation and reuses the worker only after ResetDone.
Scalar and parallel workloads each contain 0, 1, 10 or 100 echo operations.
Payloads contain 32 bytes, 8 KiB or 1 MiB minus framing headroom. Process mode
captures a continuation at every boundary, releases the worker, restores its
opaque bytes on the next checkout and completes with the same result. Guest
errors discard and replace the process; they never return it to the idle pool.

Paired reference and worker samples alternate order on every iteration. Both
parse and compile each cell. The profiler reference uses f0's `State` and `execute` boundary with its
original source workload and a current-thread executor created outside timing; historical f0 numbers remain context, not a matched control.
Resumed segments use the current owned VM interface on both sides.
The worker enforces its configured limits and serializes state across cells.
The optimized workspace graph enables worker/client/lashlang testing features;
these workloads invoke no test frontend, hook or continuation probe. The SDK
helper is packaged and verified separately without testing features.
Cold means new exec, prewarming and execution, with executable/page caches allowed
to remain hot. Warm means a prewarmed, resettable pool. Sampling is closed loop;
its throughput is completed operations divided by total measured service time,
not an arrival-rate capacity estimate. No outliers are discarded.

Raw samples are CSV. Summary JSON uses nearest-rank p50/p99/max, explicit units,
counts and service throughput. Paired overheads retain negative observations.
Exchange samples start at the worker's monotonic timestamp before request
serialization and end in the parent after the next request arrives. They include
the current request's encoding, transfer and parent decode, parent answer encoding,
reply transfer and worker decode, guest work and the next request's serialization.
Both processes use the same machine's CLOCK_MONOTONIC clock. This aligns the
value baseline's two directions with the exchange boundary. Exchanges exclude tools/journaling and are an upper
boundary for this synthetic IPC population. Raw aggregate exchanges are whole batches. Their budget rows divide each sample
by the batch leaf count and carry the explicit `per_leaf` unit and `leaves` N. Queue delay is measured from checkout invocation to
admission. Multi-cell or resumed workloads sum their checkout waits. Widths 1/2/4
measure complete concurrent short-script batches, including thread launch/join,
with workers prewarmed. A one-slot queue probe holds a checkout
until another caller has actually queued its encoded input, releases it, and
records the waiter's complete checkout delay. It adds no artificial sleep. Worker and parent RSS/HWM come from Linux procfs; RSS includes shared
pages, HWM is a process lifetime peak, and neither is PSS nor an OS memory limit.

Preregistered honesty budgets are 1 ms p50/5 ms p99 for paired warm zero-effect
overhead and 100 us p50/500 us p99 per leaf for effect exchange. Parallel-N
samples are divided by N before judging, while raw batch distributions remain
in the report. Every value population measures a paired baseline immediately
after each exchange, in the same process and run. A persistent socket-pair peer
uses the production MessagePack payload and bounded frame codecs to encode,
move and decode the identical request and answer in both directions, including
the same echo value extraction. Thread startup, fixture preparation and answer
validation are outside baseline timing. Value budgets judge exchange minus
that baseline, per sample. The baseline and signed subtraction each retain their
own p50/p99, and budget rows flag every negative sample without clamping it.

Every exchange records parent decode, parent encode, IPC write, IPC read/wait,
worker decode, worker encode and guest work until the next request. Echo host
work has its own row. IPC read/wait is the exclusive wall-time remainder after
the measured parent and worker phases and host work. It includes scheduling,
protocol bookkeeping and the timing hook's own overhead. Raw blocking reply read/wait
is retained separately, since it overlaps worker work and cannot be added to it.
Overlap exceeding the end-to-end interval is explicit. Sample-by-sample phase
sum minus overlap must reconcile with the total within 1 us; JSON and the report
show the sums, errors and overlapping sample counts. Summing phase percentiles
is not a reconciliation. Instrumentation uses const-generic hooks: ordinary
client calls and worker startup select the false specialization, which compiles
out all measurement clocks and telemetry emission. The matrix selects the
measured worker at startup and the measured effect-answer method.

All thresholds stay report-only. Nothing in CI or release gates on them.
No benchmark threshold may change to make observed results pass. Shared-host
instrumented results are diagnostic; FIG-4172 owns quiet-host final numbers.
Host presets stay explicit and inspectable; workload measurements do not
establish arbitrary-guest deadline guarantees.

RSS probes run separately, once per hundred warm observations and after each
cold run. They are outside the latency timer. Guest-error workers have already
been reaped by the pool when the terminal arrives, so their post-work RSS/HWM is
unavailable; the report must mark it pending, never replace it with zero.
