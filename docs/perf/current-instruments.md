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

## LashVm

```sh
python3 scripts/profile_lash_vm.py --scenario baseline --mode one_shot \
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

These diagnostic runs retain completed samples but exit 2 because the fast
case did not run. This is functional proof, not a passing latency gate. The default
`just latency-gate` still runs every case and requires 10,000 fast samples,
overhead p50 below 50 ms and p99 below 250 ms on a qualified quiet host.

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
