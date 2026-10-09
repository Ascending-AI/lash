# Observation workloads

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
