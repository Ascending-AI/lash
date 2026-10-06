# PostgreSQL substrate spike (FIG-5167)

This standalone lash-perf binary measures the scheduling SQL in the substrate
DDL sketch. It installs a private schema and drops it on completion. SQL is in
`sql.rs`; no product store, engine or stored format changes.

Run from a Kiln fork:

```sh
. ./env.sh
mkdir -p .kiln/FIG-5167/bin
kiln sync
kiln build --materializations final //crates/lash-perf:postgres-substrate__bin \
  --out "$PWD/.kiln/FIG-5167/bin"
kiln gate lash fig-5167 -- python3 scripts/postgres-substrate-bench.py \
  --binary .kiln/FIG-5167/bin/postgres_substrate \
  --evidence-dir .kiln/FIG-5167/pg-final -- \
  --nodes 1,4,16 --seconds 3 --wake-events 128 \
  > .kiln/FIG-5167/final.jsonl 2> .kiln/FIG-5167/final.log
```

The launcher reuses the repository's pinned PostgreSQL action runner and schema
setup. It requires `KILN_GATE_ID`, refuses an external database URL, uses an
unused loopback port and keeps cluster files under the evidence directory.
It enables fsync, synchronous commit and full-page writes. `--test-settings`
selects the runner's cheaper durability-off settings for a separate experiment.
The cluster is stopped and removed on exit; stdout retains settings, binary
SHA-256, host facts and all raw latency samples. The binary can also run against
a caller-owned database through `LASH_POSTGRES_DATABASE_URL`; it creates only
its random schema and requires permission to create/drop schemas.

## Method

One worker connection per simulated node, four runtime threads, one primary.
Each saturation phase runs three seconds after 16 warmup operations per node.
Connections and seed data are prepared outside the window; seeded scheduling
rows are analyzed before the query plan is captured. No ordinary phase holds
an idle transaction. One separate observer samples PostgreSQL lock waits every
5 ms (actual cadence includes query time and scheduling).

- Claim: the sketch's ready/waiting predicate, format filter, order, limit,
  `FOR UPDATE SKIP LOCKED`, epoch bump and returned grant. Ready sets contain
  4,096 actors (batches 1/16/64) or 16 hot actors (batch 16). Claims commit; a
  separate release statement recycles them. Claim latency excludes release;
  throughput includes it. Empty calls count in operation throughput, while
  actor throughput counts only returned rows. Tied due times have no fairness
  guarantee. The diagnostic holds eight of the hot actors locked throughout
  warmup and measurement, including an MVCC snapshot: its result includes
  sustained contention and version churn, not just the cost of skipping eight
  rows.
- Fence: BEGIN, actor-row `FOR NO KEY UPDATE`, check epoch 1, write a 256-byte
  domain row and increment its revision plus actor progress, COMMIT. Independent
  actors model ownership. The hot singleton deliberately shares a grant to
  expose the lock convoy; it is not a valid multi-owner runtime design. Fence
  lock latency includes the SELECT round trip and scheduling, not pure server
  lock time.
- Heartbeat/reap: each worker renews its node with a 15 s TTL, then runs the
  atomic delete-and-epoch-bump sweep. These saturation phases have healthy nodes
  and no expired actors. Separately, 128 populated sweeps per fleet expire all
  nodes with 16 owned actors each. Populated sweep latency excludes preparation;
  its throughput includes truncation, reseeding and post-sweep epoch validation.
- Wake: one paced producer admits 128 durable sequence rows, with deterministic
  variable gaps of 5–45 ms, then every node observes every sequence. Each node
  has a dedicated listener or independently phased polling connection. No
  notification/wake gaps or reorderings are accepted. End-to-end latency starts
  before admission; the post-commit notification metric starts before the
  separate `pg_notify` statement. All clocks are this process's monotonic clock.
  Wake throughput is the offered broadcast delivery rate, not capacity. The
  in-transaction comparison is low offered load and does not test notification
  commit-lock saturation.

Quantiles use sorted samples and nearest ranks. Warmup, setup and validation
are excluded from latency samples. Lock observations count waiting sessions
at each sample, not unique waits; zero observations do not exclude short waits.

## Measured result: 2026-10-06

`measurements-20261006.json` is the compact receipt (raw samples omitted).
The full evidence is `.kiln/FIG-5167/final.jsonl`, collected with the lane.
Pinned PostgreSQL **16.15**, 128 MiB shared buffers, `fdatasync`, all three
durability settings **on**, loopback TCP; AMD Ryzen 9 5950X (16 cores / 32
threads), about 126 GiB RAM, ext4 on local NVMe. This shared development host,
with a default Buck build and no replicas, is not a production capacity SLA.

The following is aggregate throughput across the fleet. Latencies are ms.
A heartbeat/reap operation is a pair of autocommit statements. Populated sweep
rates include setup as described above.

| Operation | Nodes | Operations/s | p50 | p99 | Samples |
| --- | ---: | ---: | ---: | ---: | ---: |
| claim_4096_batch_1 | 1 | 303.3 | 2.990 | 4.480 | 911 |
| claim_4096_batch_16 | 1 | 259.6 | 3.530 | 5.325 | 780 |
| claim_4096_batch_64 | 1 | 204.5 | 4.092 | 5.536 | 614 |
| claim_hot16_batch16 | 1 | 1154.2 | 0.509 | 0.814 | 3464 |
| claim_hot16_half_locked | 1 | 279.4 | 1.912 | 3.338 | 839 |
| fence_distinct | 1 | 2641.0 | 0.361 | 0.531 | 7929 |
| fence_hot1 | 1 | 2648.4 | 0.365 | 0.480 | 7949 |
| heartbeat_reap_empty | 1 | 4408.1 | 0.218 | 0.292 | 13234 |
| reap_populated | 1 | 208.8 | 0.512 | 0.814 | 128 |
| claim_4096_batch_1 | 4 | 1226.1 | 2.882 | 5.127 | 3684 |
| claim_4096_batch_16 | 4 | 909.1 | 3.788 | 6.602 | 2731 |
| claim_4096_batch_64 | 4 | 646.5 | 4.815 | 8.693 | 1942 |
| claim_hot16_batch16 | 4 | 10205.2 | 0.315 | 0.718 | 30635 |
| claim_hot16_half_locked | 4 | 8238.2 | 0.358 | 2.502 | 24736 |
| fence_distinct | 4 | 6817.4 | 0.567 | 0.740 | 20462 |
| fence_hot1 | 4 | 3120.1 | 1.255 | 1.769 | 9370 |
| heartbeat_reap_empty | 4 | 11837.8 | 0.320 | 0.434 | 35540 |
| reap_populated | 4 | 179.4 | 0.860 | 1.543 | 128 |
| claim_4096_batch_1 | 16 | 2862.6 | 5.163 | 7.181 | 8607 |
| claim_4096_batch_16 | 16 | 1603.1 | 8.143 | 11.225 | 4818 |
| claim_4096_batch_64 | 16 | 914.9 | 12.262 | 31.732 | 2753 |
| claim_hot16_batch16 | 16 | 16094.5 | 0.884 | 1.120 | 48328 |
| claim_hot16_half_locked | 16 | 16537.1 | 0.841 | 2.276 | 49700 |
| fence_distinct | 16 | 7185.7 | 2.220 | 2.488 | 21579 |
| fence_hot1 | 16 | 2786.8 | 6.347 | 12.098 | 8375 |
| heartbeat_reap_empty | 16 | 12028.4 | 1.282 | 1.664 | 36160 |
| reap_populated | 16 | 123.8 | 2.644 | 3.330 | 128 |

Claim actor throughput for batches 1 / 16 / 64 is respectively **303 / 4,153 /
13,088 actors/s** at one node, **1,226 / 14,546 / 41,378** at four, and **2,863 /
25,649 / 58,556** at sixteen. The broad-set plan is a sequential scan and sort
before `LockRows`, followed by primary-key updates. The sketch's OR predicate,
coalesced ordering and per-row volatile clock computation are not served as an
ordered bounded range scan by the ready partial index. Thus these rates measure
that exact sketch, not an optimized scheduler. Treat a production claim query
plan as a design input, especially with larger ready queues.

At 16 nodes on 16 hot actors, **43,438 / 48,328 calls (89.88%)** are empty;
**38,879 actors** are claimed (12,948/s). All nodes make progress: 2,110–2,985
actors per node. The observer sees **0 lock-wait session observations in 359
ticks**. With eight actors held locked, **47,327 / 49,700 calls (95.23%)** are
empty; 6,827 actors are claimed (2,272/s), and all nodes get 378–497 actors.
There are **14 lock-wait session observations in 384 ticks**. SKIP LOCKED avoids
a persistent row-lock convoy, but it does not promise every claim/release
statement is wait-free. Immediate retries mostly burn SQL on empty scans.

For the 16-node fenced singleton, SELECT p50/p99 is **5.932 / 11.634 ms** versus
**0.622 / 0.807 ms** on independent actors. The observer sees 7,088 waiting-session
observations across 500 ticks (14.18 waiting sessions per tick) on the singleton,
and zero across 421 ticks on independent actors. Its 2,787 transactions/s is a
lock convoy. Independent-actor capacity of 7,186 small transactions/s is
consistent with H6's toy estimate (~2,395 rounds/s at three such transactions per
round); it does not establish full turn throughput or a primary ceiling on other
hardware, across a network, with larger payloads or synchronous replicas.

Wake throughput counts broadcast deliveries (N deliveries per admission), and
is constrained by the producer's deliberate pacing.

| Wake mode | Nodes | Deliveries/s | End-to-end p50 ms | p99 ms | Deliveries |
| --- | ---: | ---: | ---: | ---: | ---: |
| wake_notify_in_transaction | 1 | 37.7 | 0.509 | 0.773 | 128 |
| wake_notify_after_commit | 1 | 37.6 | 0.636 | 0.856 | 128 |
| wake_poll_250ms | 1 | 36.6 | 131.454 | 246.046 | 128 |
| wake_poll_1000ms | 1 | 32.0 | 563.718 | 994.617 | 128 |
| wake_notify_in_transaction | 4 | 151.3 | 0.492 | 0.754 | 512 |
| wake_notify_after_commit | 4 | 150.3 | 0.658 | 1.045 | 512 |
| wake_poll_250ms | 4 | 141.7 | 126.629 | 247.859 | 512 |
| wake_poll_1000ms | 4 | 124.5 | 538.280 | 991.396 | 512 |
| wake_notify_in_transaction | 16 | 598.7 | 0.538 | 1.008 | 2048 |
| wake_notify_after_commit | 16 | 596.3 | 0.760 | 1.230 | 2048 |
| wake_poll_250ms | 16 | 567.0 | 125.480 | 247.695 | 2048 |
| wake_poll_1000ms | 16 | 472.5 | 475.520 | 990.083 | 2048 |

For **post-commit NOTIFY alone**, send-to-delivery p50/p99 is **0.251 / 0.333 ms**
(1 listener), **0.280 / 0.481 ms** (4), **0.407 / 0.781 ms** (16). Its statement
round-trip p50/p99 is 0.197/0.258, 0.200/0.463 and 0.493/0.877 ms. All **5,376**
notification deliveries and **5,376** polling observations arrive. In-transaction
NOTIFY saves a round trip at this offered load; these measurements do not decide
whether its commit serialization becomes a bottleneck at saturation.

LISTEN needs a direct, dedicated session or **session pooling**. PgBouncer's
[feature map](https://www.pgbouncer.org/features.html) supports LISTEN in session
pooling and excludes it from transaction pooling; NOTIFY is supported in both.
Use session pooling for listeners and a separate transaction-pooled write pool.
Keep listeners out of long transactions: PostgreSQL delivers notifications
between transactions ([PostgreSQL 16 NOTIFY documentation](https://www.postgresql.org/docs/16/sql-notify.html)).
Neither this spike nor its pooler conclusion depends on running PgBouncer.

## Recommended starting defaults

| Setting | Default | Reason |
| --- | --- | --- |
| Heartbeat interval | **3 s** | 5.33 updates/s for 16 nodes; negligible against measured healthy heartbeat/reap capacity. |
| Node lease TTL | **15 s** | Five renew opportunities; leave room for pauses and transient transport failures. This is a policy choice, not a measured failure-detector optimum. |
| Node self-stop | **10 s** since last successful renewal | Stay below TTL; a zero-row heartbeat stops immediately. Epoch checks remain the write fence. |
| Reap sweep interval | **2 s**, jittered per node | At most eight fleet sweeps/s at 16 nodes; even releasing 256 actors has 3.330 ms p99 here. |
| Claim/mail poll ceiling | **250 ms**, with independent phases | Lost hints then have roughly a 250 ms polling delay instead of the measured ~1 s tail. At 16 nodes, two idle polling paths cost at most 128 queries/s at this fixed interval. |
| Empty-claim backoff | **25 ms**, doubling to **250 ms**, jittered; wake resets it | Avoid the 90–95% empty-call storm seen with immediate hot-set retries. |
| Claim batch | **16**, capped by free execution slots | At 16 nodes, 25,649 broad-set actors/s with 11.225 ms p99; 64 raises actor throughput but p99 reaches 31.732 ms and reserves more work. Tune to 64 only for an established backlog and sufficient free slots. |
| Wake hint | Coalesced **post-commit NOTIFY**, with polling as recovery | Sub-ms notification delivery at 16 listeners. Keep the correctness transaction independent of hint delivery; saturation placement remains unmeasured. |

Keep the epoch check inside each write transaction and evict an owner's cached
state on failure. Lease expiry is liveness, not a separate commit fence. A crash
recovery budget with these settings is approximately TTL + reap + poll + claim
latency (~17.3 s here); this spike does not measure node crashes, failover,
coalescing, lost notifications, sustained vacuum/WAL growth, or a saturation
comparison of NOTIFY placement. Those remain inputs for the substrate lanes.
