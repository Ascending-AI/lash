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
Small scalar effect exchanges include parent decode/answer/encode, IPC and guest
work until the next request; they exclude tools/journaling and are an upper
boundary for this synthetic IPC population. Aggregate exchanges are whole batches,
not per-leaf round trips. Queue delay is measured from checkout invocation to
admission. Multi-cell or resumed workloads sum their checkout waits. Widths 1/2/4
measure complete concurrent short-script batches, including thread launch/join,
with workers prewarmed. A one-slot queue probe holds a checkout
until another caller has actually queued its encoded input, releases it, and
records the waiter's complete checkout delay. It adds no artificial sleep. Worker and parent RSS/HWM come from Linux procfs; RSS includes shared
pages, HWM is a process lifetime peak, and neither is PSS nor an OS memory limit.

Preregistered honesty budgets are 1 ms p50/5 ms p99 for paired warm zero-effect
overhead and 100 us p50/500 us p99 for effect exchange. Budget JSON checks every
exchange population, including whole aggregate batches and large values;
the report explains that those also contain guest work and serialization. The
report records every failing budget. These are diagnostic acceptance thresholds, not
portable CI wall-clock gates on a shared executor. No benchmark threshold may
change to make observed results pass. Host presets stay explicit and inspectable;
workload measurements do not establish arbitrary-guest deadline guarantees.

RSS probes run separately, once per hundred warm observations and after each
cold run. They are outside the latency timer. Guest-error workers have already
been reaped by the pool when the terminal arrives, so their post-work RSS/HWM is
unavailable; the report must mark it pending, never replace it with zero.
