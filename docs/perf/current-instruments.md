# Current performance instruments

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

## Load models

Every receipt names how its operations were generated in `load_model`.

| `load_model` | Arrivals | Latency clock starts | Instruments |
|---|---|---|---|
| `offered-load` | Independent, on a schedule | At the scheduled arrival | `lash-perf offered-load` |
| `service-diagnostic` | Closed loop: the next operation is sent when the last one returns | At the send | Every runtime-perf scenario (including `high_traffic_load_sqlite` and `high_traffic_knee_sqlite`), every `lash-perf latency` case, and the PostgreSQL live-replay bench |

A closed loop delays its own arrivals behind a slow operation and never
records their wait, so its tail and its knee understate queueing. Use a
`service-diagnostic` receipt to compare service time at a fixed concurrency.
Take tails, capacity and the saturation knee from an `offered-load` receipt.
`lash-perf offered-load` holds the only arrival generator.

## Offered load

```sh
E="$PWD/.kiln/<ticket>"
kiln run //crates/lash-perf:lash-perf__bin -- offered-load \
  --population high-traffic --rates 5,20,80,320 --operations 48 --sessions 4 \
  --store-dir "$E/offered-store" --out "$E/offered-load.json"
```

Run it through `kiln run`: the high-traffic turns execute in the VM worker,
which a bare materialized executable does not find. The store directory must
be fresh. The command writes the receipt and, beside it, every operation's row
in `<out>.ledger.json` (`--ledger-out` moves it).

`--rates` is an ascending sweep in operations per second. Each step opens a
fresh SQLite store and population and schedules `--operations` arrivals:
operation `n` is due `n / rate` after the step opens, whether or not earlier
operations have finished. Arrivals go to `--sessions` sessions by ordinal; a
session runs one turn at a time, so an arrival on a busy session queues with
its clock running. At most `--max-in-flight` operations are sent and not yet
completed; an arrival beyond that waits for a slot, also with its clock
running. A step closes `--drain-timeout-ms` after its last scheduled arrival
and counts what is still open as unfinished.

| Population | What one operation is |
|---|---|
| `high-traffic` | A `send()` on the runtime-perf high-traffic population: one core submits and serves, with the `--mix` of plain, tool, queued and child turns. |
| `cross-worker` | A `send()` from a core that serves no sessions, executed by a child durable node on the same SQLite file and followed to its persisted answer. |

Each ledger row has the operation's key and five timestamps, in microseconds
on the generator's monotonic clock since the step opened:

| Timestamp | Instant |
|---|---|
| `scheduled_us` | The arrival's due instant. Configured, not measured. |
| `sent_us` | The generator calls `send()`. |
| `admitted_us` | A store poller first reads the input as taken by a run. |
| `settled_us` | The store poller first reads the run's terminal. |
| `completed_us` | The send's outcome returns to the caller. |

`admitted_us` and `settled_us` are observed by a poller that backs off from
2 ms to 50 ms, so each can be up to one poll interval late. On `high-traffic`
the poller reads through the serving core's own store. A row's `outcome` is
`completed`, `error` or `unfinished`.

Each step of the receipt reports:

- `offered_rate` (configured) and `achieved_rate`, the departure rate between
  the first and the last completion, with their ratio;
- `counts` of scheduled, sent, admitted, settled, completed, failed and
  unfinished operations;
- `intervals`, one distribution per pair of timestamps. Percentiles are
  nearest-rank over the exact samples up to `p99_9` and `max`, the
  power-of-two buckets are unclipped, and `censored` counts the scheduled
  operations with no sample. `scheduled_to_completed` is the offered-load
  latency; `sent_to_completed` leaves out the wait before the send;
- `slowest`, the slowest completed operations with every timestamp, and the
  keys of the unfinished and failed ones.

Every number carries its quantity, unit, window and statistic. A `configured`
statistic is an input or an upper bound, not a measurement.

`knee` names the lowest swept rate that fell behind and the highest rate below
it that kept up. A step has fallen behind when it leaves operations
unfinished, when its achieved rate is below `--knee-min-achieved` of its
offered rate (default 0.95), or when its `scheduled_to_completed` p99 exceeds
`--knee-p99-ratio` times the lowest rate's (default 3). The knee lies between
those two rates; a finer sweep narrows it. When no step falls behind, the knee
is above the highest swept rate. A step with few operations can cross the
achieved-rate criterion on latency jitter alone.

## Runtime, stack and allocation profiles

```sh
E="$PWD/.kiln/<ticket>"
mkdir -p "$E"
python3 scripts/profile_runtime.py --release --profile quick \
  --scenario standard --runs 1 --warmups 0 --turns 1 \
  --build-report "$E/runtime-build.json" --out "$E/runtime.json"
python3 scripts/profile_runtime_stack.py --release --scenario standard \
  --stack-bytes 2097152 --runs 1 --turns 1 \
  --no-build --build-report "$E/runtime-build.json" --out "$E/stack.json"
python3 scripts/profile_runtime.py --release --profile quick --dhat \
  --scenario standard --runs 1 --warmups 0 --turns 1 \
  --build-report "$E/allocation-build.json" --out "$E/allocation.json" \
  --dhat-out "$E/allocation.dhat.json"
```

The allocation recipe selects the inventory's `dhat-heap` feature target and
uses the symbolized profiling platform. The default runtime instrument retains
its stats_alloc counters. Allocation profiles cover the measured window.
`perfreport` displays each DHAT allocation site's `gb` as
`peak_live(at_tgmax)`: requested bytes live at the profiled process's global
heap peak. Its `eb` is `end_live`: requested bytes still live when the profiler
ends. Site `mb` is that site's own maximum; `max_live(sum-of-pps)` sums those
independent maxima and is not a simultaneous global peak.

Stack receipts use `configured_stack_capacity_bytes` and
`configured_stack_capacity_source`. The source identifies the explicit Tokio
worker reservation, `RUST_MIN_STACK` thread default, or process `RLIMIT_STACK`
soft limit, in priority order. `stack_budget_bytes` is a configured constraint,
not observed usage. There is no capacity-versus-budget usage verdict. These
settings do not bound every thread or child; constrained-execution crash laws
and the stack sweep establish survival at a configured size, not occupancy.

The scheduler's `runtime.global_queue_depth_endpoint_max_tasks` is the maximum
queued-task count at the two scenario-window endpoints in the executing Tokio
runtime. It misses intervening peaks. Process CPU covers every thread in the
parent process during that window; worker busy/park metrics sum worker deltas.

Every runtime-perf scenario is a `service-diagnostic`. The high-traffic
scenarios run a closed loop per session, and `high_traffic_knee_sqlite` steps
the session population and reports p95 service-latency growth: it is not an
offered-load knee. The former `--runtime-perf-load-arrival-rate` pacing is
gone; scheduled arrivals belong to `lash-perf offered-load`.

## LashVm

```sh
python3 scripts/profile_lash_vm.py --report-only --scenario baseline --mode one_shot \
  --iterations 1 --profile-scenario baseline --profile-iterations 1 \
  --build-report "$E/lash-vm-build.json" --out "$E/lash_vm.json"
```

The script resolves `perf`, `profile` and `function_perf` independently from
one build report, rather than guessing `target/release/examples` paths.
The perf example reports allocation operations and requested-byte increments
within each counter-reset window (growing reallocations contribute their byte
increment); it emits no live-heap peak. Pre-window objects can be freed during
execution, so reset counters cannot establish a new-window live peak without
tracking allocation membership. Use DHAT for live-at-peak and live-at-end
quantities rather than deriving them from these counters.

The VM script certifies by default: any failed selected allocation, time,
scaling or opcode budget exits 1. `--enforce-budgets` selects that mode explicitly;
`just perf-guard`, release and manual perf workflows use it. `--report-only`
exits successfully after measurement and labels both output and receipt as
noncertifying. Skipped populations are not selected; a certifying invocation
must select at least one. Scaling ratios bind only when their mode and both
scenarios are selected, and missing measurements within that selection fail.

The profile example emits `vm_instructions_total` before the top-12 display.
This is the full count of executed VM opcodes in the profile subprocess's
selected execution window, not native CPU instructions. `instructions_per_iter`
divides that total by reported iterations. Standard sweeps profile each scenario
separately. **FIXED-SCENARIO-OPCODE-WORK** pins the full opcode work of one fixed
benchmark scenario from its seeded state, with zero padding. The 28 scenario
ceilings in `scripts/perf_guard_budgets.json` came from one execution of each
unchanged fixture; the baseline ceiling is 126. Hotspot timing/ranking never
contributes to this count. A fixture or bytecode change requires a reasoned
update to its named work invariant, not a shared default ceiling.

The standard runtime phase inventory includes `context_transform` and
`plugin_hook.context_pressure.standard_compaction`, required in the full geometry
of five measured runs, one warmup and twelve turns. **NESTED-PREPARATION** gives
each the existing configured `prepared_turn` upper bound of 108.842 ms: both
phases occur inside `RuntimeTurnServices::prepare`'s prepared-turn span. These
are advisory bounds inherited from the enclosing phase, not new measured timing
baselines. Existing allocation ceilings stay enforced.

## VM worker matrix

The optimized `//crates/lash-perf:vm-worker-matrix__bin` certifies by default.
It writes `budgets.json` and the exchange report before refusing any selected
failed verdict: exchange nearest-rank p50 / p99 above configured 100 / 500 us
per leaf, zero-effect paired overhead above configured 1 / 5 ms per case, or
phase reconciliation errors above the configured 1 us tolerance. Exchange
windows cover worker request serialization through the next request's arrival;
zero-effect overhead is the paired worker-minus-reference case interval.
`--exchanges N --out DIRECTORY` selects exchanges only; a full measurement also
selects zero-effect overhead. `--report-only` labels output and receipts as
noncertifying and retains samples for shared-host diagnosis. These runs do not
establish quiet-host baselines.

## Send-to-completion latency

```sh
LASH_LATENCY_ARTIFACT_DIR="$E/cross-worker" \
  just latency-gate --cases cross-worker --lanes 1 --scale-down
LASH_LATENCY_ARTIFACT_DIR="$E/poll" \
  just latency-gate --cases poll --lanes 1 --scale-down
LASH_LATENCY_ARTIFACT_DIR="$E/grace" \
  just latency-gate --cases grace --lanes 1 --scale-down
```

Each remote case launches a child durable node over one shared SQLite file.
The submitting core and marker observer serve no sessions. Child startup has
an explicit readiness handshake; closing its stdin requests node shutdown,
with a bounded kill-and-reap fallback. A child is killed if the case exits early.
The host submits only through `send()` and follows the persisted answer.

`cross-worker` uses production observer pacing. `poll` delays the terminal
wait's poll so the normal follow poll observes settlement. The historical
`grace` case extends the follow-poll ceiling to 120 seconds while preserving
the 25 ms floor used by binding discovery, alongside the remote terminal wait.
The current follower has no shift attach or live-report grace.
Both delayed paths still perform their first store read immediately. The
report's `follower` field names the current mode, and `poll_detect_ms` remains
a simulated 25 ms to 1 s schedule, not another measured clock.

Every latency case is a `service-diagnostic`: a lane sends its next sample
when the last one returns, and each span starts at the send. For
cross-worker send-to-completion under scheduled arrivals, run
`lash-perf offered-load --population cross-worker`.

These diagnostic runs retain completed samples but exit 2 because the fast
case did not run. This is functional proof, not a passing latency gate. The default
`just latency-gate` still runs every case and requires 10,000 fast samples,
overhead p50 below 50 ms and p99 below 250 ms on a qualified quiet host.
A successful child with `qualified=false` makes the load wrapper exit 3; a
failed child retains its own status. The load receipt records both
`exit_status` (child) and `certification_exit_status` (wrapper). Qualification
uses the maximum sampled 1-minute load average in the fast-case window against
the configured core-count bound; PSI is diagnostic. `just latency-gate`
propagates that status and cannot certify an unqualified run.

## Symbolized CPU sampling

The opt-in `//tools/buck2:profiling` target platform compiles first-party and
third-party runtime code at opt-level 3 with line tables, retained symbols and
frame pointers. It uses the existing optimized compile budgets and all existing
pool properties; normal and optimized configurations retain their behavior.

```sh
kiln build //crates/lash-perf:lash-perf__bin \
  --target-platforms //tools/buck2:profiling -c kiln.rust_profile=optimized \
  --materializations final --build-report "$E/cpu-build.json"
B="$(python3 tools/buck2/outputs.py --report "$E/cpu-build.json" \
  --label //crates/lash-perf:lash-perf__bin --single)"
perf record -g --call-graph fp -o "$E/perf.data" -- "$B" \
  --runtime-perf-scenario standard --runtime-perf-runs 1 \
  --runtime-perf-warmups 0 --runtime-perf-turns 1 --runtime-perf-smoke
perf report --stdio -i "$E/perf.data"
```

`--cpu-profile` selects this platform in any of the three profiling scripts.
Sampling requires host perf permissions and belongs on a quiet host. Building
and running a symbolized executable is functional proof; no CPU optimization
claim follows from a smoke receipt.

## Independent 1.0 boundaries

`lash-perf boundary` runs one population and writes a functional JSON receipt.
Build once and keep its output report; the CLI and its SQLite child writers use
that materialized executable in place:

```sh
kiln build //crates/lash-perf:lash-perf__bin --materializations final \
  --build-report "$E/boundary-build.json"
kiln run //crates/lash-perf:lash-perf__bin -- boundary \
  --case wire-slots --operations 1 --callers 2 \
  --store-dir "$E/wire-slots-store" --out "$E/wire-slots.json"
```

The store directory must be fresh. Select each case independently; there is no
aggregate population that folds unlike boundary costs into one number.

| Case | Operations and measured boundaries |
|---|---|
| `wire-slots` | One lowering per call, original resolution, provider-file upload/cache hit/invalidation, and delivery/send on each of three attempts. The synthetic provider rejects a file, fails before a response, then answers; the product derivative cache uploads twice and reuses the replacement on the final attempt. |
| `token-healthy`, `token-expiring`, `token-rejected` | `--operations` waves of `--callers` concurrent callers on one route and epoch. Host-source requests and gate waits are separate; a barrier establishes shared leases before refresh. Healthy waves never replace; expiring/rejected waves single-flight one replacement. |
| `root-redrive` | A sent root parks on an unserved profile; restoring the deployment and explicitly committing redrive mail makes it finish. Park, operator mail, and settlement have separate intervals. Root redrive currently uses the durable mail API alongside facade sends. |
| `parked-takeover` | A deferred tool call survives node shutdown with its key and deadline unchanged. Pinned-call restoration, resolution, the next owner's durable claim, and settlement are separate. Takeover spans node restart through the new claim; the fixture models graceful owner loss. |
| `sqlite-processes` | At least two OS processes open independent product stores and cores on one SQLite file before the parent releases their stdin barriers. Each writes `--operations` sends; child acceptance/provider/settlement intervals and parent boot/join intervals remain separate. |
| `pg-facade` | `--callers` facade nodes, each with its own product PostgreSQL pool/listener, share a baseline-initialized database and concurrently settle `--operations` total sends. The workload refuses a server outside major 18. |
| `typed-history` | Bounded raw-history pages plus typed committed-turn decoding and a client fold, with separate page, node, turn and entry counts. Cursor traversal must observe every sent turn exactly once. |
| `process-lifecycle` | Repeated session-turn process start, terminal await and observation with one active process at a time. Alternate waves hold the synthetic provider until explicit cancellation; successful and cancelled terminals are counted separately. |
| `seeded-plan` | `--workload smoke-v1` or `figments-v1` supplies deterministic plans for `--callers` actors and `--operations` turns each. Generated RLM cells execute tools, attachment puts and child processes through the current served durable node. Keyed queued inputs run separately. The receipt records seed/hash and lists unmeasured host-process, auxiliary-LLM, fault-window, maintenance, arrival and observation fields. |

## Observation workloads

The same `boundary` command measures process observation, session
observation, the host trace sink and the workflow execution overlay. Each case
runs real facade work over served durable nodes whose replay stores sit behind
counting probes, so a receipt states what crossed the store boundary. Without
`--postgres-url` a case runs on a SQLite file with the built-in memory replay
stores; with it, on PostgreSQL 18 product stores with the PostgreSQL live and
process replay stores, in replay schemas of its own.

| Case | `--operations` / `--callers` | Measured boundaries and counters |
|---|---|---|
| `process-dispatcher` | committed steps per process / concurrent processes | Language-observation dispatcher on one node: observations per second, store round trips per observation, the deepest ingress backlog (reconstructed from each draft's observation time), store-wide invalidations, feed gaps. |
| `process-feeds` | committed steps / open feeds | Feed snapshot, subscribe, per-item wait and recovery from a stale cursor; publications per durable commit, redeliveries, and on SQLite the statements the store ran per commit. Compare receipts across `--callers` for the cost of an open feed. |
| `process-burst` | committed steps per process / bursting processes | A quiet process is followed while the others burst. Store-wide invalidations, gaps by cause on bursting and quiet feeds, resnapshots. With PostgreSQL the quiet observer is on a second replica. |
| `process-convergence` | committed steps / unused | Two replicas each follow one process. Per-fact lag between the two consumers, gaps per replica, and each replica's publications. |
| `process-reconcile` | committed steps / unused | A consumer on a replica that executes nothing holds a snapshot from the process's start, then recovers. Facts reconcile pushed into that replica's window against the window's size and the reconcile budget, and whether the feed gapped anyway. |
| `process-roster` | roster size / concurrent starts | Roster pages unfiltered and under the running-only filter (which selects nothing here), and a change-feed scan from the start: pages, rows, empty pages, per-page latency. |
| `session-replay` | sends / observers | Live replay publications, events delivered per observer, and the window from a send's settlement to its `Committed` reaching each observer. |
| `session-resume` | sends / sessions | Resume from the cursor of every revision; then one `invalidate_all` with an observer attached to each session: observers gapped, time to each gap, and continuation past it. |
| `trace-sink-omitted`, `trace-sink-captured` | sends / unused | Per-record cost of the host `TraceSink` (a JSONL sink) under each content policy, records by kind, bytes written. |
| `trace-sink-custom` | sends and custom records / payload KiB | The same, plus host-authored `custom` records of the given size offered to the installed sink. |
| `trace-sink-otel` | sends / unused | The OpenTelemetry adapter over an SDK tracer provider: exports, spans, per-export latency. |
| `trace-sink-slow` | sends / sink delay in ms | A sink that blocks for the delay and refuses every fourth record: time the emitter was blocked, records lost, and the sends' settle latency. |
| `overlay-fold` | loop turns / step sites | The execution overlay folded from what a process's feed delivered: per-observation fold, per-observation snapshot, and the settlement, by document nodes and sites. |
| `overlay-attribution` | loop turns / branch sites per turn | One program in the VM with and without an observing host: observations per run and the median overhead per observation. No store takes part. |

Every boundary receipt carries `latency` (p50, p95, p99, max and mean per
boundary, in microseconds), `throughput` (operations over each wall-clock
window the workload measured as one), `allocations` (allocator counters over
the whole case, setup included, in the binary's allocation mode) and
`counters`. Committed effect facts stop at eight per step site, so the process
cases spread `--operations` over as many sites as that takes. A feed that ends
with an error before its terminal is counted in the receipt
(`feeds_ended_with_error`) and does not fail the case.

An executed process slows with its own length, so `process-reconcile
--workload registry-facts` instead appends `--operations` wait facts to a held
process through the process registry port. Use it for histories past the
replay window (2,048 events) or the reconcile budget (16,384 facts). Every
fleet member registers under its own node name; a shared name would fence the
earlier boot.

One command profiles any case. The scripts build the executable and the VM
worker it starts, and run the case in a fresh store directory:

```sh
python3 scripts/profile_runtime.py --release --boundary process-feeds \
  --boundary-arg=--operations=64 --boundary-arg=--callers=8 \
  --build-report "$E/boundary-build.json" --out "$E/process-feeds.json"
python3 scripts/profile_runtime.py --release --dhat --boundary process-feeds \
  --build-report "$E/boundary-dhat-build.json" --out "$E/process-feeds-alloc.json"
python3 scripts/profile_runtime_stack.py --release --boundary process-feeds \
  --stack-bytes 2m --no-build --build-report "$E/boundary-build.json" \
  --out "$E/process-feeds-stack.json"
```

`--dhat` selects the `dhat-heap` target and writes a heap
profile of the whole case beside the receipt; `--cpu-profile` selects the
symbolized platform for `perf record`. A PostgreSQL run adds
`--boundary-arg=--postgres-url=<url>` inside the service gate below. To run an
executable in place instead, set `LASH_VM_WORKER` to the materialized
`//crates/lash-vm-worker:lash-vm-worker__bin`.

The PG18 workload takes `--postgres-url`; run it under
`kiln gate lash <fork> -- scripts/ci/with-service.sh pg -- <command>`, passing
the gate's `LASH_POSTGRES_DATABASE_URL` to the executable. The service launcher
applies the product baseline. Never point this population at the sketch SQL
instrument's tables. Its receipt says `facade` and `postgres18-product`;
`durable-substrate` still measures the direct engine and `postgres-substrate`
still measures sketch SQL. These populations cannot substitute for each other.

Receipts record operations observed at real callbacks and store/API boundaries.
An interval around a caller includes queueing; nested and concurrent intervals
overlap and must not be summed. Setup and synthetic wait time are not a timing
baseline. The small laws and service smoke prove function, without timing
thresholds, repetitions, or claims about a quiet-host distribution.

## Durable transaction telemetry

`lash.durable.commit.lock_statement_elapsed` is a histogram in microseconds
for lock-bearing statements within one physical PostgreSQL transaction attempt,
labelled by commit and outcome. It includes execution and network time and is
an upper bound on lock wait. It does not isolate server lock-wait time; its
existing producer and name already preserve this distinction.

## Comparable duration history and operation tails

```sh
kiln run //crates/lash-perf:lash-perf__bin -- duration-trend \
  --history crates/lash-perf/fixtures/duration-trend-history.jsonl \
  --csv "$E/duration-series.csv"
kiln run //crates/lash-perf:lash-perf__bin -- receipt-tail \
  --receipt "$E/runtime.json" --slowest 5
# The same command reads a latency receipt or its raw sample ledger.
kiln run //crates/lash-perf:lash-perf__bin -- receipt-tail \
  --receipt "$E/cross-worker/latency.json" --slowest 5
```

`receipt-tail` prints tab-separated per-operation p50, p99, p99.9 and max in
milliseconds, contributing and missing sample counts, and the slowest raw IDs.
Percentiles interpolate at `p * (n - 1)`. Runtime IDs are receipt run indices
and retained turn indices; sampled metric IDs are raw array positions. Latency
IDs are lane/sample coordinates. These identify raw records, not product TurnIds.
Runtime first and subsequent turns are separate; high-traffic knee steps are
separate populations. Run envelopes and per-turn phase sums are explicitly named
`run_total` and `phase_total`, and are never pooled with individual operations.
Latency cases, cold/warm, statuses and poll timeouts stay separate. Signed phase
differences survive; cumulative process CPU endpoints are excluded and the
simulated poll clock is labelled. Missing marks have no fabricated zero.
A latency receipt names its actual raw ledger (including a custom destination);
`--samples <FILE>` can point to a separately retained ledger. Reading the raw
ledger directly also works. An absent ledger is an error, never a summary fallback.
Small populations describe their samples; they cannot certify rare tails.

History records now require host identity (hashed machine/hardware/kernel facts),
the actual build compiler and flags, allocator, backend, configured storage
policy, workload, and all configured workload geometry, including stack size.
Different identities remain separate, with typed `IdentityMismatch` reasons in
the table; missing identity is refused. PostgreSQL policy is read before the
measured window; a failed policy read disables comparisons of that population.
SQLite policy is the harness's configured store options. Neither is a measured
durability claim. History without the current identity shape must be regenerated;
the pre-1.0 baseline version remains unchanged.

The advisory detector retains its existing 50%/five-consecutive-run rule. Once
it detects a shift, the baseline freezes and the finding remains open through
later runs, recovery and compaction. Recent observations plus the original
bounded detection witness are retained. A review in the committed
`scripts/perf_duration_level_shifts.json` closes an accepted shift:

```json
{
  "level_shifts": [{
    "profile": "full",
    "scenario": "standard",
    "metric": "total_ms",
    "commit": "<commit that changed the level>",
    "effective_from": "2026-10-09T10:00:00Z",
    "who": "<reviewer>",
    "reason": "<why the shift is accepted or tracked as a bug>",
    "disposition": "accepted"
  }]
}
```

Use `bug` to record ownership and the reason while keeping the finding open.
`accepted` starts a new comparison window at `effective_from`; it does not mute
a later shift. Selectors are exact, and omitted selectors cover every matching
series, so narrow acknowledgements to the reviewed population. Who and why are
required. The committed file is the review trail; cache eviction still loses
observations and is never an acknowledgement.

Existing `_ms` metric summaries and `metric_summary_ms` distributions both enter
history. The latter use `sampled_operations/` names so pooled operation medians
cannot overwrite per-run metric medians. The CSV carries quantities, units,
window, statistics and full identities for offline review. The trend remains
advisory, and neither this reader nor a receipt establishes a quiet-host baseline
or guarantees someone has read a warning.

## Runtime fixture reach and counted graph curves

The following reduced selections exercise the named fixtures through the real
`send()` and served durable engine. Run each selector independently:

```sh
kiln run //crates/lash-perf:lash-perf__bin -- \
  --runtime-perf-scenario rlm_large_print --runtime-perf-runs 1 \
  --runtime-perf-warmups 0 --runtime-perf-turns 1 --runtime-perf-smoke \
  --runtime-perf-out "$E/rlm_large_print.json"
```

The same geometry applies to `rlm_large_tool_catalog`,
`rlm_tool_catalog_cold`, `rlm_tool_catalog_warm`, `embed_standard` and
`embed_rlm`. Embedding retains the serving core until every sent turn and the
state export finish; the session handle alone does not own the serving node.

Large print retains its 70-line text and sixteen repeated rows. Its receipt's
`rlm.configured_instruction_limit_per_cell` is the configured 8,000,000-unit
fuel bound, not a measured opcode count: the VM also charges proportional
string work and graph imports/exports. Other RLM fixtures retain their
1,000,000-unit bound. The catalog fixtures retain the full tool population and
configure their session prompt plan for it. The receipt labels the configured
limits as `prompt.configured_section_bytes_limit_per_call` (524,288 bytes per
section) and `prompt.configured_total_bytes_limit_per_call` (1,048,576 bytes
across the rendered request); product defaults remain unchanged.

```sh
kiln run --config=optimized //crates/lash-perf:lash-perf__bin -- \
  --runtime-perf-scenario frame_residency_curve_sqlite --runtime-perf-runs 1 \
  --runtime-perf-warmups 0 --runtime-perf-turns 1 --runtime-perf-smoke \
  --runtime-perf-out "$E/frame_residency_curve_sqlite.json"
kiln run --config=optimized //crates/lash-perf:lash-perf__bin -- \
  --runtime-perf-scenario resident_graph_append_curve --runtime-perf-runs 1 \
  --runtime-perf-warmups 0 --runtime-perf-turns 1 --runtime-perf-smoke \
  --runtime-perf-out "$E/resident_graph_append_curve.json"
```

The SQLite frame curve sweeps 0, 1,000, 8,000 and 32,000 prior-history rows with
64 current-frame rows. Its verdict requires exactly 64 decoded rows per reopen,
bounded retained heap bytes, and exactly 64 submitted graph rows in every
sampled commit. `frame_residency.prior_<n>.commit_graph_rows_max` counts the
maximum submitted graph rows per commit in that point's measured commit window;
it does not claim to count SQL statements or database rows written. Raw
`frame_residency.prior_<n>.commit_ms` samples and their medians remain elapsed
milliseconds for diagnosis, with no shared-host timing-ratio verdict.

The resident graph curve retains every allocation slope cap, including 48
allocated bytes per resident node for snapshot append. A rejection emits
`resident_graph_rejected_run=<JSON>` on stderr using the existing run receipt
shape, then exits nonzero. Its phase entries count allocations and allocated
bytes in the named operation window, with `samples` as the divisor; the slope
subtracts the zero-resident mean before dividing by resident nodes. This makes
product regressions reviewable without changing their verdict. FIG-5659
attributes the snapshot-append cap crossing to the versioned indexes introduced
by `ac19148e6f` (FIG-4060); it remains a product-owned regression.

## Direct durable-substrate process waits

```sh
kiln run //crates/lash-perf:durable-substrate__bin -- \
  --store sqlite --case process-waits-10 --samples 1 \
  --sqlite-dir "$PWD/.benchmarks/process-waits-10" \
  --out "$E/process-waits-10.jsonl"
```

Use a fresh SQLite directory. Deployment publishes the content-addressed
process-execution environment before wrapping stores for recording or starting
measurement. The receipt therefore measures real process registration and ten
waits rather than refusing on a missing environment artifact. The same setup
serves the parked-process and idle-process populations; this reduced command
exercises only `process-waits-10`.

## Startup phases and loopback provider HTTP

```sh
kiln run //crates/lash-perf:lash-perf__bin -- startup \
  --store-dir "$PWD/.buck2/startup-population" --out "$E/startup.json"
kiln run //crates/lash-perf:lash-perf__bin -- provider-http \
  --requests 6 --rates 10,100,1000 --body-bytes 128,8192 \
  --max-inflight 2 --server-parallel 1 \
  --first-chunk-delay-ms 2 --chunk-delay-ms 1 --chunk-bytes 1024 \
  --retry-every 2 --out "$E/provider-http.json"
```

`startup` launches concurrent fresh-process waves at widths 1, 2 and 4. Each
child opens a fresh SQLite file with `Normal` synchronous mode, builds a durable
node, observes its registration, creates a session, prewarms one real VM helper,
and submits its first loopback-provider turn through `send()`. Each process uses
two configured Tokio workers. The existing `latency worker ready` handshake
still marks core construction; node registration is a separate marker. Use a
fresh `--store-dir` for each run; existing database files are refused. OS page
and executable caches are uncontrolled, and no cache is dropped.

The opt-in `perf_witness::startup` recorder retains the first of each named
phase, sharing one monotonic epoch initialized at process entry. It joins the
first connection open/setup, store-set completion, core construction, durable
node registration, session creation, VM spawn/spawned/ready, first provider
request, and first committed send result. VM markers retain the helper PID and
are parent observations. Child clocks are never subtracted from parent launch
clocks. Parent launch/ready/exit intervals and child entry-to-first-result
samples remain separate in the receipt. Width summaries preserve raw markers,
completion counts, finite-wave rate, and the first width whose first-result p95
is at least 1.25 times width 1. This configured diagnostic knee criterion is
not a shared-host performance gate or sustainable startup-capacity claim.

`provider-http` extends the existing loopback OpenAI-compatible fixture with
prescribed first-body/interchunk delays, chunk sizes, server parallelism and
one scripted 503 on each selected call before its successful SSE retry. It runs
real `ProviderHandle` reliability and parsing, explicitly permitting one bounded
retry in this fixture with no billing, against two configured user/answer
text sizes and independently scheduled arrival rates. Every finite arrival is
recorded, including generator rejection at `--max-inflight`, provider error,
actual offer/admission/completion, and scheduled-to-completion latency. Offered
rate uses the actual offer window; achieved rate uses the completion window.
The generator never waits for an earlier call before scheduling its next
arrival. The fixture uses plain loopback HTTP without a proxy, a client per
population, and `Connection: close`; it never contacts a live provider.

The reusable `lash_http_transport::observation` decorator is content-free and
opt-in. A bounded ledger belongs to one logical provider call; its HTTP ordinal
joins that call's sealed attempt ordinal and optional usage, so retry bytes and
latency stay attached to the correct attempt. Request-built means the completed
`HttpRequest` entering the transport seam, excluding earlier provider lowering.
Headers, first nonempty transport chunk, body end and offered/delivered body
bytes use monotonic samples. End distinguishes EOF, failure and early drop.
First chunk is not first visible model token, and the decorator cannot split
DNS/TLS/connect or measure kernel wire bytes. It retains no second response
body and preserves the existing body-budget checks. Usage is synthetic fixture
input/output/cache/reasoning token data; missing attempt usage remains unknown.

Both receipts define quantity, unit, process/window and statistic for their
numbers, retain raw samples, and declare `functional_noncertifying`. Small
shared-host populations establish phase/attempt/accounting completeness only;
quiet-host repeated populations are required for timing and capacity decisions.
These populations do not overlap the observation, trace-sink or workflow-overlay
instruments.
