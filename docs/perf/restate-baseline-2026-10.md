# Restate runtime baseline, 2026-10-06

FIG-5168 / substrate L12a. This records the current production runtime path
through `send()`, the engine's drive, live Restate, and PostgreSQL storage.
The fixtures and the [raw ledger](restate-baseline-2026-10.json) are committed
for L12b to repeat on the same machine and build profile. There are **64
completed batches, 1,144 answered turns, and 10 scenario shapes**.

The one-tool round median is 119.874 ms for a one-round turn and 101.828 ms
within a twenty-round turn. Completion after suspension takes 332.183,
447.078 and 1,054.000 ms at turn-run journal lengths 66, 162 and 522.
The suspended process completes 511.719 ms after its external completion.
These are observations; the prospect's performance hypotheses remain hypotheses.

## Machine and versions

- Host: `turbo2-sam`, AMD Ryzen 9 5950X, 16 cores / 32 logical CPUs,
  131,804,228 KiB RAM; Linux `6.8.0-137-generic`, x86-64.
- Source base: `d55354b40771f4a681588e7a90f02e551ae64447`, with this ticket's
  additive perf fixtures. No production runtime, stored format, or epoch changed.
- Rust: checksum-pinned Kiln toolchain, `rustc 1.98.1 (48a229cea 2026-09-01)`.
  Buck2 **default developer profile**, first-party `opt-level=0`, `debuginfo=0`;
  `stats_alloc` allocator. Use this same profile for the substrate comparison.
  An optimized release comparison needs a separate optimized baseline.
- Restate: pinned `restate-server 1.7.12`, commit `577e032`, from the existing
  then-current Restate launcher. That launcher is retired; `just latency-gate`
  now measures lash's own durable engine.
  One node/partition, loopback TCP, 256 MB RocksDB budget, ordinary live-leg
  invocation timing. No forced-replay leg.
- PostgreSQL: Docker `postgres:16-alpine`, **16.15**, immutable image digest
  `sha256:721873c34ceb9f8d8fc265984940dc982404c105f19ad51be9fdc5970a6080ea`.
  `fsync=on`, `synchronous_commit=on`, `full_page_writes=on`,
  `shared_preload_libraries=pg_stat_statements`, `max_locks_per_transaction=256`.
  Each scenario gets a fresh provisioned database. PostgreSQL allows 100
  connections in run A and 512 in runs B–D. Runtime pools have minimum 4,
  maximum 32, except maximum 256 for the 100-session case.
- Host, worker, Restate and PostgreSQL run on this machine. It is shared;
  the recorded one-minute load before/after runs A–D was respectively
  1.65/7.82, 2.90/27.76, 12.88/15.55 and 6.32/4.92. The ledger retains the
  full three load averages. No quiet-host qualification or isolation is claimed.

## Method

A deterministic provider returns immediately. Each normal turn issues N
rounds of `benchmark_echo`, then a text answer; a concurrency turn uses five
rounds with three parallel echo calls each. Every batch opens fresh sessions
before concurrently sending one input per session. Session creation and
teardown are outside the measured interval. All outcomes must be `Answered`,
and the fixture checks the model-call count against the requested rounds.

A **step** in these tables is one tool round. Its latency is the elapsed time
between successive provider entries, including tool work, journal/store work,
and the next model request. The first-model and final-answer tails are separate
fields in the ledger. Total latency is the direct host `send()` to `outcome()`
clock; it includes both tails. Throughput is completed turns divided by the sum
of batch measurement windows, excluding creation, teardown and report writing.
Percentiles use nearest rank. The ten-turn p99 is the sample maximum; it does
not establish a population tail bound.

The resume fixture performs N rounds, then a deferring tool whose completion
key stays unresolved. The harness observes the **turn's run handler suspended**
in Restate before resolving that key. It holds the confirmed suspension for
another second. The process fixture uses the shipped RLM TypeScript channel to
start a VM process that awaits that same host tool, observes the **process run
handler suspended**, and holds it for another 30 seconds. At that boundary the
process workflow's own run journal has 31 entries, its enclosing turn has 71,
and all namespace journals total 325. One observation is recorded per suspension
case; these rows have no statistical percentile claim.

Resume latency begins immediately before `core.completions().resolve(...)` and
ends when the waiting host receives the outcome. It includes completion ingress,
Restate redelivery/journal replay, remaining runtime work and the host follower.
It measures resuming a suspended invocation on the existing deployment. A worker
kill, cold machine restart, failover and replay-decode-only timing were outside
this measurement. The roughly 60 seconds spent reaching actual suspension are
included in the full-send column and excluded from the resume column.

SQL writes are the delta of `pg_stat_statements.calls` for this database's
statements containing SQL `INSERT`, `UPDATE` or `DELETE` tokens, including
write-bearing CTEs and zero-row writes. Counter-observation queries are explicitly
excluded. This is a **statement count**, not transaction, WAL, fsync or modified-row
count. The ledger's `returned_rows` is PostgreSQL's reported row count; a CTE's
returned rows cannot be interpreted as every row it modified.

Journal writes are the delta of `sys_invocation.journal_size` summed across
**all** services in the scenario's namespace, retaining completions/notifications
as Restate counts them. Suspension rows retain the actual invocation identities,
service names, handler names, status and journal sizes. SQL and journal snapshots
are taken before sends and after their handler tails settle, before session close.
Teardown is then awaited before another batch. These counters include runtime
polling/recovery and scheduling work in the window. Journal and SQL counts are
reported separately because they represent different operations. Per-round write
counts divide measured turn totals by N, so fixed turn overhead is amortized;
they do not isolate just a tool body's writes.

## Measurements

Tool round and complete-turn latency (milliseconds):

| Tool rounds | Turns | Round p50 | Round p99 | Total p50 | Total p99 |
|---:|---:|---:|---:|---:|---:|
| 1 | 10 | 119.874 | 167.125 | 360.789 | 476.609 |
| 5 | 10 | 105.579 | 150.994 | 765.005 | 963.034 |
| 20 | 10 | 101.828 | 139.915 | 2319.040 | 2805.029 |

Write counts, averaged over completed turns:

| Rounds | SQL statements/turn | Journal entries/turn | SQL/round | Journal/round | Combined/round |
|---:|---:|---:|---:|---:|---:|
| 1 | 92.60 | 281.00 | 92.60 | 281.00 | 373.60 |
| 5 | 125.60 | 624.20 | 25.12 | 124.84 | 149.96 |
| 20 | 272.90 | 1915.40 | 13.64 | 95.77 | 109.42 |

Suspension and external completion (one sample per row; milliseconds):

| Prior tool rounds | Turn run journal at suspension | All journal entries at suspension | Time parked after suspension | Completion request to outcome | Full send to outcome |
|---|---:|---:|---:|---:|---:|
| 1 | 66 | 257 | 1000.766 | 332.183 | 61812.967 |
| 5 | 162 | 609 | 1000.971 | 447.078 | 62571.866 |
| 20 | 522 | 1885 | 1001.229 | 1054.000 | 64494.258 |
| process | 71 | 325 | 30001.631 | 511.719 | 91414.760 |

Concurrent sessions, each with five rounds of three parallel tools:

| Sessions | Pool maximum | Batches | Completed turns | Total p50 ms | Total p99 ms | Round p50 ms | Round p99 ms | Turns/s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 32 | 10 | 10 | 1171.587 | 1474.448 | 178.694 | 248.142 | 0.85 |
| 10 | 32 | 10 | 100 | 1668.896 | 1958.159 | 260.050 | 556.099 | 5.81 |
| 100 | 256 | 10 | 1000 | 11611.930 | 13019.464 | 1784.890 | 2359.189 | 8.46 |

## Commands and retained evidence

This is a historical Restate measurement, not a recipe for the current
engine. Its runtime launcher and report scripts were retired with Restate.
The committed [ledger](restate-baseline-2026-10.json) retains commands,
machine versions and completed samples; those commands require the historical
source revision. Use [current instruments](README.md) for new
receipts over lash's own durable engine, and label comparisons by engine,
source revision and build profile.

The retained run sequence was A (53 batches of rounds/resumes and concurrency
1/10), B (six 100-session batches), C (four more 100-session batches), and
D (the process-only case with the corrected XML channel fence).

A ended while opening 100 live sessions because the original 32-connection
fixture pool timed out. B retained six answered batches before an unpaginated
admin query exceeded the client's 16 MiB response limit. C retained four answered
batches before the process fixture's Markdown fence was treated as prose. D
completed successfully. Those harness failures do not enter the sample population.
The final harness sizes the 100-session pool, pages administration reads within
the existing response limit, and emits the actual `<typescript>` channel fence.
Earlier setup attempts and their logs remain under `.kiln/FIG-5168/attempt-*`;
their numbers are excluded. Counters, outcome checks and percentile definitions
were identical for the retained successful batches.

The historical launcher used private PostgreSQL and Restate services, and
stopped them after retaining the completed samples. Database URLs and
credentials are absent from the committed ledger. There is no current Restate
reproduction entrypoint.

The committed ledger preserves every completed turn's timings, each batch's
write counts, journal bounds, suspension observations, commands and machine
versions. Repeated historical invocation lists are compacted into their observed
journal bounds. Full lists and setup failures remain in the lane's collected
`.kiln/FIG-5168/` evidence. Workspace Clippy passed; no correctness tests were
added or changed by this measurement-only ticket.
