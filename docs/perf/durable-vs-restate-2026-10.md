# Durable substrate against the Restate baseline, 2026-10-07

**H1 passes at engine level, and H5 passes for the specified RLM cell. H2 passes as restated by FIG-5206: a cold resume costs a fresh turn plus O(current turn), and its checkpoint no longer grows with prior turns. Since FIG-5207 a turn reads its session window from the store once, not twice.** H5 also fails outside its scenario, once a cell keeps about 16 KiB per tool result.

FIG-5188 / substrate L12b. Every number here comes from a run recorded below,
on the same machine and the same default developer build profile as L12a's
[Restate baseline](restate-baseline-2026-10.md). The raw ledger is
[durable-substrate-2026-10.jsonl](durable-substrate-2026-10.jsonl): every
turn, batch, resume, block, process, idle and store record. The tables below
are its output under `report.py`.

**PostgreSQL version.** These numbers were taken on PostgreSQL 16.15, the
version the repository pinned then. Lash 1.0 supports and tests PostgreSQL 18
only (FIG-5209); the PostgreSQL rows here were not re-measured on 18.

**What these numbers include.** On main, no host can run a turn through
`send()` yet: `LashCore` builds no `SessionActivation`, and L3's facade wiring
and L9h's follow-up pass are pending. So these runs drive the **production
durable engine** directly. They use the node runner
(`runtime::durable::node::serve`), the session activation and phase runner, the
round runner, cells, the process activation, waits and session close, over the
production SQLite and PostgreSQL stores with `DurableSettings::default()`. Only
the protocol, the model (it answers at once), the tool bodies and the process
engine belong to the bench. The facade's own work is **not** in these numbers:
`send()`, the tool registry, projections and the event feed. L12a's figures
include it. So every comparison below compares the engine plus store against
the full Restate path. The H1 verdict says how much facade cost would still
pass the gate.

## Verdicts

- **H1 (engine overhead per tool round ≥2× lower): pass, at engine level.**
  On PostgreSQL with one node, the median round falls from 119.9 / 105.6 /
  101.8 ms to 8.8 / 10.0 / 12.3 ms for 1 / 5 / 20 rounds (8.3–13.7× lower).
  The p99 falls from 167.1 / 151.0 / 139.9 ms to 14.7 / 16.2 / 20.1 ms
  (6.9–11.3×). With four nodes the factors are 8.0–9.4× at p50 and 7.0–9.8× at
  p99; on SQLite they are 21–51× and 11–34×. The model and the tools take no
  time here (L12a's provider was immediate as well), so a round is all engine
  overhead. Headroom: the facade could add 38.3 ms per round at p50
  (PostgreSQL ×4, 20 rounds), or 49.8 ms at p99 (PostgreSQL ×1, 20 rounds),
  before the 2× gate fails on the worst row. A round commits two transactions: `model.done`, then
  `round.outcome`. A third, `round.present+model.start`, opens the next model
  call. That is design-opus §6's "about 3 PG transactions per round".
- **H5 (VM snapshot per block <5 ms p99 for the RLM cell): pass for the
  specified cell; fail for heaps of 16 KiB per result.** The cell awaits ten
  host operations (design §6 (c)) and keeps every answer. With answers up to
  1 KiB, a block's snapshot is 5.9–25.4 KB. The transaction that commits it
  (`cell.snapshot+admit`, which also carries the broker ledger and the next
  operation's admission) takes 3.72 / 4.92 ms p99 on PostgreSQL, with fsync,
  and 0.52 / 0.61 ms on SQLite. VM capture plus encode, re-measured with S1's
  bench on today's code (optimized), is 0.025 ms p99 for a small cell and
  0.755 ms for a 257-frame process. With 16 KiB per answer the snapshot is
  202–292 KB, and its commit reaches 7.06 ms p99 on PostgreSQL. VM capture
  plus encode is 3.19 ms p99 for 10k numbers and 28.1 ms for 10k records.
  S1's case for chunked large heaps therefore stands, but the gate as
  specified passes.
- **H2 (cold resume costs a fresh turn plus O(current turn)): pass after
  FIG-5206.** A turn held at its model call is restored by a fresh node, with
  nothing cached, from its committed checkpoint. L12b measured it failing a
  stricter bar, "flat in prior turns": its checkpoint embedded the whole
  window the turn started from (2.9 KiB growing to 437.9 KiB after 300 prior
  turns). FIG-5206 makes the checkpoint pin that window by the session head it
  started from and hold only the turn's own delta; a restore reads the window
  at the pin. The checkpoint is now 3.1–3.2 KiB at 0 / 10 / 100 / 300 prior
  turns on both dialects. Claim to `turn.commit` is 41.6 / 46.6 / 92.2 /
  161.4 ms on PostgreSQL (was 37.5 / 38.3 / 92.8 / 172.4) and 9.1 / 13.6 /
  43.5 / 117.7 ms on SQLite (was 9.9 / 12.3 / 46.9 / 133.2). The remaining
  slope is not the resume's: instrumented at 300 prior turns on SQLite (900
  window messages, this debug profile), `restore_from_checkpoint` takes
  1–2 ms; reading the window at the pin takes 44–51 ms (the store's window
  read 36–40 ms, adopting it 6–9 ms); the bench's `finish` reads the head
  window again for the head commit (47–69 ms); and the pin check hashes the
  O(prompt) request (4–8 ms). A fresh turn pays the same: its start reads the
  head window (47–61 ms) and its `finish` reads it again. The model call needs
  the window, so a cold resume reads it once, as a fresh turn does; "flat in
  prior turns" was the wrong bar for a bench that never compacts.
  FIG-5207 removes the second window read and the repeated request hash. The
  session actor keeps the head it loaded in its owner cache, keyed by actor
  and epoch. The turn's start or restore reads its window from that head,
  and its head commit is the store's compare-and-set against that head, with
  no re-read. The model-call pin reuses the request digest the checkpoint
  already computed. Re-run A/B on one host against main `a6cbd94a65`, claim
  to `turn.commit` p50 at 0 / 10 / 100 / 300 prior turns drops by 8.1 / 17.4 /
  38.6 / 72.6 ms on PostgreSQL (39.9 / 48.9 / 94.8 / 164.0 to 31.8 / 31.5 /
  56.2 / 91.4) and by 1.9 / 4.4 / 20.1 / 54.3 ms on SQLite (8.9 / 13.3 / 43.4 /
  115.6 to 7.0 / 8.8 / 23.3 / 61.3). At 300 prior turns on SQLite that is the
  instrumented `finish` read (47–69 ms) plus the hash (4–8 ms). The remaining
  slope is the one inherent window read.
- **H7 (failover): as designed.** With the liveness lock, a turn killed with
  `kill -9` mid-model-call is detected in 259 ms and its model call is re-sent
  290 ms after the kill. Without the lock (lease only) detection takes
  15,964 ms and the re-send comes 16,160 ms after the kill. A cleanly stopped
  node releases in 4 ms and its turn is claimed 64 ms later. design-opus said
  about 1 s, 15–20 s and about 0.
- **H8 (idle cost): as designed, flat in waiting actors.** One idle node makes
  4.8 durable transactions/s (claim polls, heartbeats, reaps), which is 32.6
  PostgreSQL statements/s and 0.73 KiB/s of WAL. With 1,000 waiting processes
  that is 4.8 transactions/s and 32.5 statements/s. Four nodes make 19.2–19.3
  transactions/s and 130 statements/s either way. A waiting actor holds no
  connection and costs 4.3–4.4 KiB of PostgreSQL relations (indexes
  included), or 2.9 KiB of SQLite file.

## Regressions above 20% p99

**No row matched to L12a regresses.** That covers tool rounds, complete turns,
SQL write statements per turn, resume, the parked process and concurrency. The
regressions found are against S2's sketch tables and under conditions L12a did
not run:

1. **Store port against S2's sketch (FIG-5167): every operation but
   `claim_4096_batch_1` at one node regresses.** The p99 rises 39–293% for the
   broad claim, 809–1970% for the hot-16 claim, 205–430% for fence, 420–535%
   for heartbeat+reap, and 114–159% for wake end to end. Hypothesis (from the
   code path, not measured statement by statement): every port write is a
   guarded transaction. It runs `BEGIN`, the fleet-fence admit, the bounded
   timeouts-and-clock statement, its own statements (claims also check the
   node is live) and `COMMIT` with fsync. S2 timed one statement. A fence is
   also two round trips: `begin` is a fenced read, then the commit
   transaction. The hot-16 claim adds 16-node contention on 16 rows (62% empty
   claims at 16 nodes). Wake publish to delivery is in line with S2: 0.39–0.44
   ms p50 and 0.57–0.75 ms p99, against S2's 0.25–0.41 and 0.33–0.78 ms.
2. **Sessions left open exhaust a node's slots: SQLite 100 sessions, p99
   58,891 ms against Restate's 13,019 ms.** This happened in a first run
   without L12a's per-batch teardown, kept under `.kiln/FIG-5188/final/sqlite-no-teardown/`.
   Hypothesis: an idle session keeps its slot for `idle_evict` (60 s). Once
   earlier batches' idle sessions fill `max_active` (256), a new session's
   claim waits for an eviction. Batches 2, 5 and 7 had 44, 88 and 32 sessions
   wait about 57 s for their first model call. With teardown, the p99 is
   1,070 ms.
3. **Checkpoint bytes are quadratic within a turn.** This is not a latency
   regression, but a 20-round turn writes 454 KiB of checkpoints (169 KiB of
   PostgreSQL WAL), against 6.1 KiB for one round. Hypothesis: each round's
   checkpoint re-embeds every message of the turn. FIG-5206 removed the
   prior window from each checkpoint (H2), not the turn's own messages, and
   these sessions start with an empty window, so this row is unchanged.
4. **A host's resolution carries no wake hint.** This is not a regression
   (243–252 ms against Restate's 512 ms), but a parked process on one node
   resumes at the claim poll's 250 ms ceiling, and on four nodes in 21–75 ms.
   Hypothesis: `waits::resolve_host` commits through
   `backend.durable().commit_mail`, not `Backend::commit_mail`. So it
   publishes no post-commit hint, and the waiting actor is found only by the
   next claim poll. With its owner still holding it, a process resumes 1.8
   (SQLite) to 5.2 ms (PostgreSQL) after a resolution.

## Machine and versions

- Host: `turbo2-sam`, AMD Ryzen 9 5950X, 16 cores / 32 logical CPUs,
  131,804,228 KiB RAM, Linux `6.8.0-137-generic`, x86-64. It is the same host
  as L12a and is shared: the one-minute load averages were 3.4–6.1 around
  the SQLite runs and 2.0–8.9 around the PostgreSQL cases (recorded per case
  in `host.jsonl`). No isolation is claimed.
- Source: origin/main `ddf7627327` plus this lane's bench. No production code
  changed.
- Rust: the checksum-pinned Kiln toolchain, in the Buck2 **default developer
  profile** (first-party `opt-level=0`) with the `stats_alloc` allocator, as
  in L12a. Bench binary SHA-256
  `5140cb4c204a168e8ad6cc32d7429a32a90eeb83d761ae65cf1d412c3b248b94`. The
  committed source differs from it only by three items Clippy found dead
  afterwards (an unused node name, an unused kill helper and an engine
  report nothing read), and by wording. The
  optimized profile is used only for the S1 VM-snapshot re-run, as S1 did.
- PostgreSQL: the repository's pinned `native//:postgres`, **16.15** (L12a
  used the Docker `postgres:16-alpine` image, also 16.15). The settings were
  `fsync=on`, `synchronous_commit=on`, `full_page_writes=on`,
  `shared_preload_libraries=pg_stat_statements`,
  `max_locks_per_transaction=256` and `max_connections=512`, over loopback
  TCP. Each case gets a fresh cluster and database provisioned with the 1.0
  baseline `schema.sql`. Each node's pool is minimum 4, maximum 32 (L12a's);
  for 100 sessions it is maximum 256 at one node and 64 per node at four.
- SQLite: one database file per case, through `SqliteStoreSet::open`.
- Substrate: `DurableSettings::default()`, which is a 250 ms claim-poll
  ceiling, 25 ms claim backoff, claim batch 16, `max_active` 256,
  `idle_evict` 60 s, a 5 ms group-commit window, 15 s lease TTL, 3 s
  heartbeat and 2 s reap, with the `AfterCommit` notifier (wake hints and the
  liveness lock) on PostgreSQL.

## Method

The definitions are L12a's. A **round** is the time between successive model
entries, so it includes the tool, the round's commits and the next model
call. The model answers at once: N rounds of `benchmark_echo` (three in
parallel in the concurrency shape), then text. Both `benchmark_echo` and the
cell's `ext.echo` are `Once` tools that answer their arguments. **Total** runs
from the start of the producer's admission commit (a session mail through a
node's own backend, as a host's `send()` in that process would commit) to the
recorder seeing `turn.commit`. Each batch opens fresh sessions, admits one turn
on each at once, and afterwards closes them outside the window. Percentiles
are nearest-rank over pooled samples, and a ten-sample p99 is the maximum.

**Writes.** "Durable txns" counts every write transaction through the durable
port, by label: owner commits, mail commits, and the lease and claim calls.
On PostgreSQL, write statements and rows use L12a's `pg_stat_statements`
filter, "all statements" includes reads, and WAL is the insert-position delta.
On SQLite the measure is the growth of the database file plus its WAL. Trailing
writes get 50 ms after the last `turn.commit`. Claim polls and heartbeats inside
the window count, as L12a counted Restate's polling.

**Resume.** L12a suspended a turn in Restate on a deferring tool. A durable
turn has no such suspension: no turn-level deferred tool exists before the
facade's tool port. So the nearest equivalent is measured instead: the
turn's node stops while it is in the model call after N rounds, and a freshly
booted node, with a new backend and pool and nothing cached, claims the
session, restores the turn from its committed checkpoint and finishes it.
"Fresh boot to commit" includes the node's registration and its first claim.
Each sample adds a turn to the session, so `prior_turns` rises within a case.

**Parked process.** A host engine pins a custom key and awaits it, and its
owner releases it. It stays parked for 30 s; then a host resolves the key
(`waits::resolve_host`), through another node's backend when there is one.
The measurement runs from the resolution to the engine seeing it, and to
`process.terminal`.

**Cell.** The model's first answer is a TypeScript cell that awaits
`ext.echo({i, pad})` ten times and pushes each answer to an array. The bench
records every transaction that writes a VM snapshot: its bytes, its commit time
and the time to the next block.

**Idle.** K processes wait on keys that nobody resolves. After they settle (a
sample of 20 was confirmed released), the bench measures a 30 s quiet window.

**Store.** S2's shapes run on the PostgreSQL store's own `DurableStore` and
`NodeWakes`, not on the spike's tables. Each simulated node is a registered boot
with its own pool. A claimed actor is released as `waiting`, already due, so
the claimable set keeps its size. Fence is `begin` plus an empty commit on the
node's own actor. Wake is a mail commit to an actor another node owns, then
the commit's post-commit hint published to that owner's listener.

## Measurements

### Tool rounds

| Store | Tool rounds | Turns | Round p50 | Round p99 | Total p50 | Total p99 | Baseline round p50 / p99 | Round p50 / p99 lower by |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| PostgreSQL x1 | 1 | 10 | 8.772 | 14.730 | 31.926 | 64.800 | 119.874 / 167.125 | 13.7x / 11.3x |
| PostgreSQL x1 | 5 | 10 | 9.950 | 16.223 | 81.175 | 124.013 | 105.579 / 150.994 | 10.6x / 9.3x |
| PostgreSQL x1 | 20 | 10 | 12.301 | 20.135 | 282.300 | 382.121 | 101.828 / 139.915 | 8.3x / 6.9x |
| PostgreSQL x4 | 1 | 10 | 12.818 | 17.125 | 45.416 | 70.463 | 119.874 / 167.125 | 9.4x / 9.8x |
| PostgreSQL x4 | 5 | 10 | 12.216 | 16.128 | 98.359 | 115.078 | 105.579 / 150.994 | 8.6x / 9.4x |
| PostgreSQL x4 | 20 | 10 | 12.656 | 19.949 | 304.115 | 359.598 | 101.828 / 139.915 | 8.0x / 7.0x |
| SQLite file | 1 | 10 | 2.368 | 4.908 | 9.455 | 24.335 | 119.874 / 167.125 | 50.6x / 34.1x |
| SQLite file | 5 | 10 | 2.541 | 6.420 | 22.624 | 33.038 | 105.579 / 150.994 | 41.6x / 23.5x |
| SQLite file | 20 | 10 | 4.760 | 13.118 | 123.147 | 148.199 | 101.828 / 139.915 | 21.4x / 10.7x |

### Writes per turn

| Store | Rounds | Durable txns/turn | of which lease/claim | PG write statements/turn | PG write rows/turn | PG statements/turn (all) | WAL KiB/turn | SQLite KiB/turn | Checkpoint KiB/turn | Baseline SQL statements/turn |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| PostgreSQL x1 | 1 | 10.10 | 3.10 | 39.20 | 35.10 | 164.90 | 18.8 | - | 6.1 | 92.60 |
| PostgreSQL x1 | 5 | 22.40 | 3.40 | 79.50 | 75.00 | 335.00 | 47.1 | - | 42.3 | 125.60 |
| PostgreSQL x1 | 20 | 68.90 | 4.90 | 230.70 | 225.20 | 972.80 | 168.8 | - | 453.9 | 272.90 |
| PostgreSQL x4 | 1 | 16.80 | 9.80 | 46.20 | 35.20 | 208.10 | 20.2 | - | 6.1 | 92.60 |
| PostgreSQL x4 | 5 | 30.70 | 11.70 | 87.10 | 75.20 | 382.30 | 49.0 | - | 42.3 | 125.60 |
| PostgreSQL x4 | 20 | 81.20 | 17.20 | 243.00 | 225.70 | 1050.40 | 171.6 | - | 453.9 | 272.90 |
| SQLite file | 1 | 9.10 | 2.10 | - | - | - | - | 1.6 | 6.1 | 92.60 |
| SQLite file | 5 | 21.10 | 2.10 | - | - | - | - | 4.8 | 42.3 | 125.60 |
| SQLite file | 20 | 67.70 | 3.70 | - | - | - | - | 118.9 | 453.9 | 272.90 |

### Transactions by label

| Store | Rounds | Owner and mail transactions per turn, by label |
|---|---:|---|
| PostgreSQL x1 | 1 | `mail.session` 1, `model.done` 1, `model.start` 1, `round.outcome` 1, `round.present+model.start` 1, `turn.admit` 1, `turn.commit` 1 |
| PostgreSQL x1 | 5 | `mail.session` 1, `model.done` 5, `model.start` 1, `round.outcome` 5, `round.present+model.start` 5, `turn.admit` 1, `turn.commit` 1 |
| PostgreSQL x1 | 20 | `mail.session` 1, `model.done` 20, `model.start` 1, `round.outcome` 20, `round.present+model.start` 20, `turn.admit` 1, `turn.commit` 1 |
| PostgreSQL x4 | 1 | `mail.session` 1, `model.done` 1, `model.start` 1, `round.outcome` 1, `round.present+model.start` 1, `turn.admit` 1, `turn.commit` 1 |
| PostgreSQL x4 | 5 | `mail.session` 1, `model.done` 5, `model.start` 1, `round.outcome` 5, `round.present+model.start` 5, `turn.admit` 1, `turn.commit` 1 |
| PostgreSQL x4 | 20 | `mail.session` 1, `model.done` 20, `model.start` 1, `round.outcome` 20, `round.present+model.start` 20, `turn.admit` 1, `turn.commit` 1 |
| SQLite file | 1 | `mail.session` 1, `model.done` 1, `model.start` 1, `round.outcome` 1, `round.present+model.start` 1, `turn.admit` 1, `turn.commit` 1 |
| SQLite file | 5 | `mail.session` 1, `model.done` 5, `model.start` 1, `round.outcome` 5, `round.present+model.start` 5, `turn.admit` 1, `turn.commit` 1 |
| SQLite file | 20 | `mail.session` 1, `model.done` 20, `model.start` 1, `round.outcome` 20, `round.present+model.start` 20, `turn.admit` 1, `turn.commit` 1 |

### Concurrency

| Store | Sessions | Batches | Completed turns | Total p50 ms | Total p99 ms | Round p50 ms | Round p99 ms | Turns/s | Baseline total p99 / turns/s |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| PostgreSQL x1 | 1 | 10 | 10 | 114.620 | 137.570 | 15.765 | 22.468 | 8.73 | 1474.448 / 0.85 |
| PostgreSQL x1 | 10 | 10 | 100 | 117.590 | 172.768 | 14.887 | 19.525 | 76.33 | 1958.159 / 5.81 |
| PostgreSQL x1 | 100 | 10 | 1000 | 589.018 | 973.732 | 69.539 | 114.134 | 142.70 | 13019.464 / 8.46 |
| PostgreSQL x4 | 1 | 10 | 10 | 116.975 | 173.111 | 16.018 | 26.034 | 8.26 | 1474.448 / 0.85 |
| PostgreSQL x4 | 10 | 10 | 100 | 124.042 | 168.935 | 14.857 | 23.782 | 71.57 | 1958.159 / 5.81 |
| PostgreSQL x4 | 100 | 10 | 1000 | 628.822 | 944.263 | 73.965 | 105.343 | 136.06 | 13019.464 / 8.46 |
| SQLite file | 1 | 10 | 10 | 39.109 | 44.980 | 4.471 | 12.541 | 27.29 | 1474.448 / 0.85 |
| SQLite file | 10 | 10 | 100 | 91.439 | 112.686 | 12.297 | 19.974 | 97.19 | 1958.159 / 5.81 |
| SQLite file | 100 | 10 | 1000 | 828.509 | 1070.312 | 125.573 | 217.937 | 95.75 | 13019.464 / 8.46 |

### Cold resume after N rounds

L12b's rows, before FIG-5206: their checkpoints still embedded the prior
window, so the first / last sizes grow with the case's prior turns (0–9).

| Store | Case | Rounds before hold | Prior turns | Samples | Checkpoint KiB (first / last) | Claim to commit p50 / max ms | Fresh boot to commit p50 / max ms | Resumed `turn.commit` p50 ms |
|---|---|---:|---|---:|---:|---:|---:|---:|
| PostgreSQL x1 | resume-1 | 1 | 0–9 | 10 | 2.9 / 15.5 | 36.8 / 47.6 | 84.0 / 110.4 | 13.3 |
| PostgreSQL x1 | resume-5 | 5 | 0–9 | 10 | 7.1 / 44.7 | 41.2 / 50.8 | 88.6 / 98.2 | 14.4 |
| PostgreSQL x1 | resume-20 | 20 | 0–9 | 10 | 22.9 / 153.8 | 63.4 / 83.1 | 111.4 / 133.2 | 21.9 |
| SQLite file | resume-1 | 1 | 0–9 | 10 | 2.9 / 15.5 | 8.9 / 10.7 | 26.7 / 28.3 | 2.8 |
| SQLite file | resume-5 | 5 | 0–9 | 10 | 7.1 / 44.7 | 12.6 / 17.9 | 28.8 / 35.7 | 3.1 |
| SQLite file | resume-20 | 20 | 0–9 | 10 | 22.9 / 153.8 | 24.0 / 50.6 | 40.6 / 66.9 | 5.6 |

### Cold resume after prior turns (H2)

Re-run by FIG-5207 on its change (see Commands); L12b's and FIG-5206's rows
are quoted in the H2 verdict.

| Store | Case | Rounds before hold | Prior turns | Samples | Checkpoint KiB (first / last) | Claim to commit p50 / max ms | Fresh boot to commit p50 / max ms | Resumed `turn.commit` p50 ms |
|---|---|---:|---|---:|---:|---:|---:|---:|
| PostgreSQL x1 | prior-0 | 1 | 0–4 | 5 | 3.1 / 3.2 | 31.8 / 34.3 | 75.4 / 78.0 | 12.5 |
| PostgreSQL x1 | prior-10 | 1 | 10–14 | 5 | 3.2 / 3.2 | 31.5 / 33.1 | 74.3 / 76.6 | 11.3 |
| PostgreSQL x1 | prior-100 | 1 | 100–104 | 5 | 3.2 / 3.2 | 56.2 / 68.6 | 98.7 / 111.6 | 15.6 |
| PostgreSQL x1 | prior-300 | 1 | 300–304 | 5 | 3.2 / 3.2 | 91.4 / 104.3 | 134.1 / 145.8 | 12.6 |
| SQLite file | prior-0 | 1 | 0–4 | 5 | 3.1 / 3.2 | 7.0 / 7.6 | 21.8 / 22.4 | 2.3 |
| SQLite file | prior-10 | 1 | 10–14 | 5 | 3.2 / 3.2 | 8.8 / 9.7 | 23.0 / 23.5 | 2.6 |
| SQLite file | prior-100 | 1 | 100–104 | 5 | 3.2 / 3.2 | 23.3 / 26.2 | 37.1 / 40.1 | 2.8 |
| SQLite file | prior-300 | 1 | 300–304 | 5 | 3.2 / 3.2 | 61.3 / 63.2 | 75.4 / 76.9 | 4.6 |

### Processes

| Store | Samples | Pin to release ms | Parked s | Resolve commit p50 ms | Resolve to engine p50 / max ms | Resolve to `process.terminal` p50 / max ms | Baseline completion to outcome |
|---|---:|---:|---:|---:|---:|---:|---:|
| PostgreSQL x1 | 5 | 22.0 | 30.0 | 2.05 | 243.0 / 246.7 | 251.8 / 255.5 | 511.7 |
| PostgreSQL x4 | 5 | 21.7 | 30.0 | 2.41 | 29.7 / 51.2 | 41.7 / 75.0 | 511.7 |
| SQLite file | 5 | 21.2 | 30.0 | 0.76 | 241.7 / 248.2 | 243.5 / 250.6 | 511.7 |

| Store | Resolutions | Hot resolve to engine p50 / p99 ms |
|---|---:|---:|
| PostgreSQL x1 | 30 | 5.178 / 8.773 |
| PostgreSQL x4 | 30 | 4.460 / 7.462 |
| SQLite file | 30 | 1.805 / 2.351 |

### Cell snapshots (H5)

| Store | Case | Blocks | Snapshot bytes p50 / max | Snapshot commit p50 / p99 ms | Block cycle p50 / p99 ms | Turn total p50 ms |
|---|---|---:|---:|---:|---:|---:|
| PostgreSQL x1 | cell-0 | 110 | 5,865 / 7,662 | 1.895 / 3.719 | 5.358 / 10.364 | 85.1 |
| PostgreSQL x1 | cell-1024 | 110 | 18,167 / 25,424 | 2.772 / 4.923 | 8.715 / 11.299 | 125.0 |
| PostgreSQL x1 | cell-16384 | 110 | 202,490 / 291,667 | 3.285 / 7.056 | 16.240 / 28.008 | 206.8 |
| SQLite file | cell-0 | 110 | 5,865 / 7,662 | 0.291 / 0.520 | 2.102 / 8.708 | 32.6 |
| SQLite file | cell-1024 | 110 | 18,167 / 25,424 | 0.385 / 0.608 | 3.231 / 5.264 | 48.4 |
| SQLite file | cell-16384 | 110 | 202,490 / 291,667 | 0.875 / 3.448 | 11.871 / 19.256 | 139.5 |

### Idle (H8)

| Store | Waiting actors | Window s | Durable txns/s | by label (per window) | PG statements/s | PG WAL KiB/s | Connections | Stored KiB per waiting actor |
|---|---:|---:|---:|---|---:|---:|---:|---:|
| PostgreSQL x1 | 0 | 30 | 4.83 | `node.claim` 120, `node.heartbeat` 10, `node.reap` 15 | 32.6 | 0.73 | 8 | - |
| PostgreSQL x1 | 1000 | 30 | 4.80 | `node.claim` 119, `node.heartbeat` 10, `node.reap` 15 | 32.5 | 0.72 | 10 | 4.30 |
| PostgreSQL x4 | 0 | 30 | 19.23 | `node.claim` 477, `node.heartbeat` 40, `node.reap` 60 | 130.2 | 2.89 | 28 | - |
| PostgreSQL x4 | 1000 | 30 | 19.27 | `node.claim` 478, `node.heartbeat` 40, `node.reap` 60 | 130.0 | 2.91 | 29 | 4.38 |
| SQLite file | 0 | 30 | 4.83 | `node.claim` 120, `node.heartbeat` 10, `node.reap` 15 | - | - | - | - |
| SQLite file | 1000 | 30 | 4.80 | `node.claim` 119, `node.heartbeat` 10, `node.reap` 15 | - | - | - | 2.92 |

### Store (S2 re-run)

| Operation | Nodes | Ops/s | Actors/s | Empty claims | p50 ms | p99 ms | S2 sketch p50 / p99 ms | p99 change vs S2 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| claim_4096_batch_1 | 1 | 221 | 221 | 0% | 3.233 | 3.988 | 2.990 / 4.480 | -11% |
| claim_4096_batch_16 | 1 | 38 | 607 | 0% | 5.159 | 9.047 | 3.530 / 5.325 | +70% |
| claim_4096_batch_64 | 1 | 14 | 917 | 0% | 7.332 | 8.455 | 4.092 / 5.536 | +53% |
| claim_16_batch_16 | 1 | 35 | 560 | 0% | 4.778 | 7.402 | 0.509 / 0.814 | +809% |
| fence_distinct | 1 | 971 | 0 | 0% | 0.974 | 1.983 | 0.361 / 0.531 | +273% |
| heartbeat_reap_empty | 1 | 847 | 0 | 0% | 1.173 | 1.519 | 0.218 / 0.292 | +420% |
| wake_notify_after_commit | 1 | 0 | 0 | 0% | 1.802 | 2.216 | 0.636 / 0.856 | +159% |
| claim_4096_batch_1 | 4 | 530 | 530 | 0% | 5.554 | 8.879 | 2.882 / 5.127 | +73% |
| claim_4096_batch_16 | 4 | 109 | 1737 | 0% | 8.853 | 18.727 | 3.788 / 6.602 | +184% |
| claim_4096_batch_64 | 4 | 33 | 2098 | 0% | 11.517 | 34.198 | 4.815 / 8.693 | +293% |
| claim_16_batch_16 | 4 | 327 | 1200 | 20% | 6.748 | 10.333 | 0.315 / 0.718 | +1339% |
| fence_distinct | 4 | 2125 | 0 | 0% | 1.841 | 2.256 | 0.567 / 0.740 | +205% |
| heartbeat_reap_empty | 4 | 1699 | 0 | 0% | 2.338 | 2.755 | 0.320 / 0.434 | +535% |
| wake_notify_after_commit | 4 | 0 | 0 | 0% | 1.503 | 2.239 | 0.658 / 1.045 | +114% |
| claim_4096_batch_1 | 16 | 843 | 843 | 0% | 13.395 | 22.449 | 5.163 / 7.181 | +213% |
| claim_4096_batch_16 | 16 | 127 | 2032 | 0% | 15.854 | 28.268 | 8.143 / 11.225 | +152% |
| claim_4096_batch_64 | 16 | 35 | 2214 | 0% | 26.634 | 44.005 | 12.262 / 31.732 | +39% |
| claim_16_batch_16 | 16 | 933 | 854 | 62% | 13.622 | 23.188 | 0.884 / 1.120 | +1970% |
| fence_distinct | 16 | 1846 | 0 | 0% | 8.228 | 13.197 | 2.220 / 2.488 | +430% |
| heartbeat_reap_empty | 16 | 1922 | 0 | 0% | 8.054 | 10.204 | 1.282 / 1.664 | +513% |
| wake_notify_after_commit | 16 | 0 | 0 | 0% | 1.472 | 2.718 | 0.760 / 1.230 | +121% |

Wake at 1 owner node(s), 128 deliveries: mail commit p50/p99 1.346/1.648 ms; publish to delivery p50/p99 0.444/0.658 ms.

Wake at 4 owner node(s), 512 deliveries: mail commit p50/p99 1.128/1.667 ms; publish to delivery p50/p99 0.391/0.566 ms.

Wake at 16 owner node(s), 2048 deliveries: mail commit p50/p99 1.057/1.919 ms; publish to delivery p50/p99 0.428/0.746 ms.

### VM capture plus encode (S1's bench re-run, optimized, 100 samples)

| Workload | Bytes | Capture + encode p50 / p99 ms | Decode + restore p50 / p99 ms |
|---|---:|---:|---:|
| small-cell | 2,179 | 0.014 / 0.025 | 0.041 / 0.067 |
| deep-process (257 frames) | 174,301 | 0.710 / 0.755 | 2.632 / 2.752 |
| numbers-10000 | 671,053 | 1.916 / 3.186 | 11.580 / 18.938 |
| records-10000 | 2,297,053 | 20.702 / 28.090 | 61.273 / 72.833 |
| numbers-100000 | 6,701,055 | 20.367 / 23.727 | 145.253 / 167.770 |

The two history cases now stop with "snapshot size changed without guest
progress". The bench's fixture has drifted from today's projection code
(L7p), so they are not reported.

### Failover (H7)

| Case | Detect ms | Claim after reap ms | Re-sent after kill ms |
|---|---:|---:|---:|
| `kill -9` mid-model-call, liveness lock | 259 | 2 | 290 |
| `kill -9` mid-model-call, lease only | 15,964 | 174 | 16,160 |
| Clean stop | release 4 | handover 64 | – |

These come from the `lash-postgres-workers` runbook's own cases, one run each
(OS-process nodes over a pinned PostgreSQL with that harness's `fsync=off`
settings).

## Commands and retained evidence

Everything ran from `/workspace/kiln/lash/forks/fig-5188` after `. ./env.sh`.
The evidence lives in that fork's `.kiln/FIG-5188/`. `F` is `$PWD/.kiln/FIG-5188/final/sqlite`
and `B` is `.kiln/FIG-5188/bin/durable_substrate`.

```sh
kiln sync
python3 tools/buck2/bootstrap_native_tools.py
kiln build //crates/lash-perf:durable-substrate__bin --materializations final \
  --build-report .kiln/FIG-5188/build-report.json
cp "$(python3 tools/buck2/outputs.py --report .kiln/FIG-5188/build-report.json \
  --label //crates/lash-perf:durable-substrate__bin --single)" "$B"

# SQLite file, one node (kiln run builds the same binary)
kiln run //crates/lash-perf:durable-substrate__bin -- --store sqlite --sqlite-dir "$F/db-a" \
  --case rounds-1 --case rounds-5 --case rounds-20 --case concurrent-1 --case concurrent-10 \
  --case concurrent-100 --case cell-0 --case cell-1024 --case cell-16384 --samples 10 --out "$F/sqlite.jsonl"
kiln run //crates/lash-perf:durable-substrate__bin -- --store sqlite --sqlite-dir "$F/db-b" \
  --case resume-1 --case resume-5 --case resume-20 --samples 10 --out "$F/sqlite.jsonl"
kiln run //crates/lash-perf:durable-substrate__bin -- --store sqlite --sqlite-dir "$F/db-c" \
  --case prior-0 --case prior-10 --case prior-100 --case prior-300 --samples 5 --out "$F/sqlite.jsonl"
kiln run //crates/lash-perf:durable-substrate__bin -- --store sqlite --sqlite-dir "$F/db-d" \
  --case parked-process --case idle-0 --case idle-1000 --samples 5 --park-seconds 30 \
  --idle-seconds 30 --out "$F/sqlite.jsonl"
kiln run //crates/lash-perf:durable-substrate__bin -- --store sqlite --sqlite-dir "$F/db-e" \
  --case process-waits-10 --samples 3 --out "$F/sqlite.jsonl"

# PostgreSQL: a fresh private cluster per case
G="kiln gate lash fig-5188 -- python3 crates/lash-perf/src/bin/durable_substrate/bench.py --binary $B --evidence-dir .kiln/FIG-5188/final/pg"
$G --nodes 1 --case rounds-1 --case rounds-5 --case rounds-20 --case concurrent-1 --case concurrent-10 \
  --case cell-0 --case cell-1024 --case cell-16384 --case resume-1 --case resume-5 --case resume-20 -- --samples 10
$G --nodes 1 --case concurrent-100 -- --samples 10 --pool-max 256
$G --nodes 1 --case prior-0 --case prior-10 --case prior-100 --case prior-300 -- --samples 5
$G --nodes 1 --case parked-process --case idle-0 --case idle-1000 -- --samples 5 --park-seconds 30 --idle-seconds 30
$G --nodes 1 --case process-waits-10 -- --samples 3
$G --nodes 4 --case rounds-1 --case rounds-5 --case rounds-20 --case concurrent-1 --case concurrent-10 -- --samples 10
$G --nodes 4 --case concurrent-100 -- --samples 10 --pool-max 64
$G --nodes 4 --case parked-process --case idle-0 --case idle-1000 -- --samples 5 --park-seconds 30 --idle-seconds 30
$G --nodes 4 --case process-waits-10 -- --samples 3
kiln gate lash fig-5188 -- python3 crates/lash-perf/src/bin/durable_substrate/bench.py --binary "$B" \
  --evidence-dir .kiln/FIG-5188/final/store --case store -- --store-nodes 1,4,16 --store-seconds 3 --wake-events 128

# H7: the runbook's failover cases, once
kiln test //crates/lash-postgres-workers:failover__test --test_output all --no-test-cache --test_arg=--exact \
  --test_arg=a_turn_killed_mid_model_call_finishes_on_another_node \
  --test_arg=a_turn_killed_mid_model_call_without_the_liveness_lock_finishes_within_the_lease_bound \
  --test_arg=a_cleanly_stopped_node_hands_its_turn_over_at_once --test_arg=--nocapture

# H5: S1's VM bench on today's code
kiln run --config=optimized //crates/lash-perf:vm-snapshot__bin -- --out "$PWD/.kiln/FIG-5188/vm-snapshot-optimized.json" \
  --samples 100 --case small-cell --case deep-process --case history-10 --case history-10000 \
  --case numbers-10000 --case records-10000 --case numbers-100000

# The ledger and these tables
python3 crates/lash-perf/src/bin/durable_substrate/report.py --inputs docs/perf/durable-substrate-2026-10.jsonl
```

The ledger concatenates `final/sqlite/sqlite.jsonl`,
`final/pg/postgres-nodes{1,4}.jsonl` and `final/store/postgres-nodes1.jsonl`,
dropping only session names. The superseded first SQLite run (without
teardown), the smoke runs and every log stay under `.kiln/FIG-5188/`.

### H2 re-run (FIG-5206)

From `/workspace/kiln/lash/forks/fig-5206` after `. ./env.sh`, on origin/main
`1dead3802d` plus FIG-5206's change, same host and profile (bench binary
SHA-256 `f6644aafdc6758ab6876a8ef63701784ebc48b76f981e075b7298ee2be545979`;
one-minute load 3.6–4.5 around the SQLite run, 1.3–2.7 around PostgreSQL).
The ledger's `prior-*` records (resume, summary and run) were replaced by
this run's; every other record is L12b's. Evidence, the per-phase timing
probe included, is in that fork's `.kiln/FIG-5206/`.

```sh
B=.kiln/FIG-5206/bin/durable_substrate
kiln build //crates/lash-perf:durable-substrate__bin --materializations final \
  --build-report .kiln/FIG-5206/build-report.json
cp "$(python3 tools/buck2/outputs.py --report .kiln/FIG-5206/build-report.json \
  --label //crates/lash-perf:durable-substrate__bin --single)" "$B"
$B --store sqlite --sqlite-dir "$PWD/.kiln/FIG-5206/h2/sqlite/db-c" --case prior-0 --case prior-10 \
  --case prior-100 --case prior-300 --samples 5 --out "$PWD/.kiln/FIG-5206/h2/sqlite/sqlite.jsonl"
kiln gate lash fig-5206 -- python3 crates/lash-perf/src/bin/durable_substrate/bench.py --binary "$B" \
  --evidence-dir .kiln/FIG-5206/h2/pg --nodes 1 --case prior-0 --case prior-10 --case prior-100 \
  --case prior-300 -- --samples 5
```

### H2 re-run (FIG-5207)

From `/workspace/kiln/lash/forks/fig-5207` after `. ./env.sh`, on the same
host and profile, as an A/B: `B0` is origin/main `a6cbd94a65` (bench binary
SHA-256 `d941cca1e8efd8b19c07b3a5f244e79f374de2ed2b41727d39af03e838bbe19f`)
and `B` is FIG-5207's change on it (SHA-256
`7d7b73109068b8c4c42fc90f8f465d8bbf1a291d190dd7cb92195f43a6ebb905`). Each
dialect ran `B0` and then `B` back to back; the one-minute load was 1.4–1.8
around SQLite and 1.4–3.3 around PostgreSQL. The ledger's `prior-*` records
(resume, summary and run) were replaced by `B`'s. Evidence, `B0`'s runs
included, is in that fork's `.kiln/FIG-5207/h2/`.

```sh
kiln build //crates/lash-perf:durable-substrate__bin --materializations final \
  --build-report .kiln/FIG-5207/build-report.json
cp "$(python3 tools/buck2/outputs.py --report .kiln/FIG-5207/build-report.json \
  --label //crates/lash-perf:durable-substrate__bin --single)" "$B"
# B0 is the same build on a checkout of a6cbd94a65; D=$PWD/.kiln/FIG-5207/h2
"$B0" --store sqlite --sqlite-dir "$D/sqlite-base/db-c" --case prior-0 --case prior-10 \
  --case prior-100 --case prior-300 --samples 5 --out "$D/sqlite-base/sqlite.jsonl"
"$B" --store sqlite --sqlite-dir "$D/sqlite/db-c" --case prior-0 --case prior-10 \
  --case prior-100 --case prior-300 --samples 5 --out "$D/sqlite/sqlite.jsonl"
G="kiln gate lash fig-5207 -- python3 crates/lash-perf/src/bin/durable_substrate/bench.py"
$G --binary "$B0" --evidence-dir .kiln/FIG-5207/h2/pg-base --nodes 1 --case prior-0 \
  --case prior-10 --case prior-100 --case prior-300 -- --samples 5
$G --binary "$B" --evidence-dir .kiln/FIG-5207/h2/pg --nodes 1 --case prior-0 \
  --case prior-10 --case prior-100 --case prior-300 -- --samples 5
```

## Not measured, and why

- **The facade-level comparison under L12a's own names**, through `send()`, the
  real tool registry and the RLM TypeScript channel. No facade turn exists on
  main yet. When L3's facade wiring and L9h's follow-up land, re-running L12a's
  shapes through `send()` adds the facade's cost, which H1 has 38.3 ms per
  round of p50 headroom to absorb.
- **A turn suspended on a deferred tool**: there is no turn-level deferred
  tool on the durable path yet. The cold-resume rows stand in for it.
- **Restate journal entries** have no durable counterpart. Owner and mail
  transactions are their nearest equivalent.
