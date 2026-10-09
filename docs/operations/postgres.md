# PostgreSQL host configuration

One validated `PostgresHostConfig` sizes, guards and names every PostgreSQL
connection a lash process opens (FIG-5240). This page is its reference: the
entry points, every section with its defaults, the role routing, the sizing
formula, the pooler rules, and the migration from hand-built pools.

PostgreSQL 17 and 18 are supported; 18 is primary.

## Entry points

```rust
use lash::postgres::{PostgresEndpoints, PostgresHost, PostgresHostConfig};

let endpoints = PostgresEndpoints::from_url(&database_url)?;
let config: PostgresHostConfig = serde_json::from_str(&config_json)?;
let host = PostgresHost::connect(&endpoints, &config, observer).await?;
let backend = lash::durable::DurableBackendBuilder::postgres(&host, attachments)
    .process_engine(engine)
    .build()?;
// host.live_replay: Some(store) when `live_replay` is configured.
// host.process_replay: Some(store) when `process_replay` is configured.
// host.pool_metrics.snapshot(): each role pool's size, idle and maximum.
```

- `PostgresHost::connect(endpoints, config, observer)` validates the
  configuration, checks the declared deployment budget against the server,
  opens the storage's role pools and, when `live_replay` or `process_replay`
  is set, that replay store. It returns `{ storage, live_replay,
  process_replay, effective_config, pool_metrics }`.
- `DurableBackendBuilder::postgres(&host, attachments)` builds the durable
  backend under `host.effective_config.node`. The durable settings live in
  that one place: calling `.config(...)` on this builder is refused at build
  with `DurableBuildError::SettingsOwnedByHost`.
- `PostgresStorage::connect(endpoints, config, observer)` opens the storage
  alone; `PostgresStorage::from_pool_set(pools, config, observer)` opens it
  over pools the host built itself (see [Imported pools](#imported-pools)).
- `PostgresStorage::migrate(endpoints, config, phase)` and
  `plan_migrations` run `lash migrate` under `config.maintenance`.
  `PostgresStorePreflight::connect_lazy(endpoints, config)` is the read-only
  probe.
- `lashctl` reads the same document from `LASH_POSTGRES_CONFIG` (JSON; the
  standard production preset when unset) and the endpoint from `LASH_POSTGRES_DATABASE_URL`.

Validation runs before any I/O and refuses the first broken rule with
`PostgresHostConfigError { field, reason }`, `field` being the serialized
path (`roles.work.max_connections`).

## Named presets

`PostgresHostConfig::standard()` is the production preset and `Default`.
JSON section omission and an unset `LASH_POSTGRES_CONFIG` resolve to it.
All operational fields remain optional and configurable. Live replay and
process replay stay absent unless selected; enabling one states the host's
retention policy for it.

`PostgresHostConfig::development()` is explicit: work/critical pools 4/1,
store admission 4, preflight/migration pools 1 each, checkpoint chunks 256
and event-release pages 32. Its node uses `DurableSettings::development()`:
claim batch 2, active actors 8, group members 8, cascade batch 32. Every
other value below is standard, including compatibility enforcement and TLS
inheritance. No universal workload measurement backs the reduced capacities.

Standard pool recycling follows SQLx 0.8.6. FIG-5167 measured wake behavior
with the standard leases. Exact role capacities, deadlines, retry counts,
reconnect delays and working chunks have no universal workload measurement;
measure and override them for the deployment. None is a refusal ceiling.

## Endpoints and credentials

The configuration holds no secret and no endpoint. `PostgresEndpoints` holds
the primary endpoint every pooled role uses and, optionally, a session
endpoint (`with_session_url`) for the roles that need a server session of
their own. Its `Debug` form, and every report lash builds from it, names only
host, port and database. Arbitrary `PgConnectOptions` are accepted through
`PostgresEndpoints::new` for TLS identities or options a URL cannot carry.

## The document

Serialized with durations as whole milliseconds (`*_ms`); an unknown field
is refused; an absent section takes its defaults. A leaf policy whose
defaults depend on its role (a pool, a guard profile, a retry, a reconnect,
a dedicated connection) and the `node` section are given whole when given at
all, so a partial object never takes another role's defaults. A server
timeout is `"inherit"` (install nothing: the server, role or database setting
applies), `"disabled"` (install `0`), or a number of milliseconds.

### `connection`

| Field | Default | Meaning |
|---|---|---|
| `application_name_prefix` | `lash` | Each connection's `application_name` is `<prefix>/<role>`. 1–32 of `[A-Za-z0-9_-]`. |
| `transport.ssl_mode` | endpoint's | `disable` … `verify_full`. Production hosts usually want `verify_full`. |
| `transport.ssl_root_cert`, `ssl_client_cert`, `ssl_client_key` | endpoint's | Certificate files; client cert and key together. |
| `statement_cache_capacity` | 100 | Prepared statements cached per connection; 0 disables. |
| `topology` | `direct` | `direct` (or a session-mode pooler) or `transaction_pool`. |

### `roles`

| Role | Default | Serves |
|---|---|---|
| `work` | 16 connections, 30 s acquire | Store calls and ordinary durable commits. |
| `scheduler` | 1, 500 ms acquire | Claim, adoption (owned scans), node drain, liveness probes. |
| `critical` | 3, 2 s acquire | Reap, node release, hand-back (`drain.release`), `turn.cancel`, `process.cancel`, `process.terminal`. |
| `renewal` | one per served node, 500 ms acquire | Node registration and heartbeat. |
| `listener` | one session per served node, 2 s open | Wake hints (`LISTEN`) and the node's liveness lock. |
| `served_nodes` | 1 | Nodes this process serves over one storage. A listener beyond it is refused with `DurableError::NodeCapacityExceeded`. |
| `max_store_operations` | 16 | Durable engine operations on the work pool at once, 1 to `work.max_connections`. |

A pool policy is `{max_connections, min_connections, acquire_timeout_ms,
idle_timeout_ms, max_lifetime_ms, test_before_acquire}`; pooled roles default
to `min 0`, `idle 600000`, `lifetime 1800000`, `test_before_acquire true`.
`null` idle or lifetime really disables recycling. Dedicated connections
(renewal, listener) are never recycled for idleness or age.

The routing is lash's: a host sizes the roles and cannot move a commit
between them. `max_store_operations` admission is taken before the work
checkout and released with the connection; it is never held across a tool or
model body. The rest of `work` serves store API calls.

### `guards`

Each profile is `{lock_timeout, statement_timeout,
idle_in_transaction_timeout, transaction_timeout, operation_deadline_ms}`.

| Profile | lock / statement / idle / operation | Runs |
|---|---|---|
| `ordinary` | 10 s / 30 s / 30 s / 60 s | Store transactions and reads on the work pool. |
| `durable` | 2 s / 5 s / 5 s / 10 s | Durable commits on the work and critical pools. |
| `renewal` | 250 ms / 1 s / 1 s / 2 s | Registration and heartbeat. |
| `scheduler` | 250 ms / 1 s / 1 s / 2 s | Claims and the scheduler's other operations. |
| `replay` | 10 s / 30 s / 30 s / 60 s | Live replay and process replay transactions. |
| `inspection` | 5 s / 30 s / 30 s / 30 s | Schema verification sessions. |

`transaction_timeout` (PostgreSQL 17+) inherits by default. `store_startup_ms`
(30 s) bounds the whole open.

Every guarded transaction sends its profile with its `BEGIN`, in the same
round trip (`BEGIN; SET LOCAL lock_timeout = …`), so the limits hold before
its first lock: the writer fence's share lock on the fleet-format row
included, and whoever built the pool. On a direct topology each role also
installs its profile as session defaults through the startup packet, which
bounds reads outside a transaction. The operation deadline is the client's
bound on the whole operation, admission and checkout included; an operation
past it fails unavailable (its commit's outcome unknown), and a lock wait past
`lock_timeout` fails contended.

Validation: `lock < statement <= operation` where both are limits; every
runtime profile has an operation deadline; `roles.renewal.acquire_timeout <
guards.renewal.operation_deadline < node.lease.heartbeat_every`, and
`heartbeat_every + renewal deadline < self_stop_after`.

### `node`

The durable substrate's `DurableSettings`, unchanged: `lease` (`ttl_ms`
15000, `heartbeat_every_ms` 3000, `self_stop_after_ms` 10000,
`reap_every_ms` 2000, `claim_poll_ms` 250, `claim_backoff_ms` 25,
`startup_ms` 2000 for registration plus the listener's start, and
`shutdown_ms` 2000 for the final release and listener close),
`claim_batch` 16, `max_active` 256, `idle_evict_ms` 60000,
`activation_loop_budget` 8, `group_commit` (`max_members` 64, `window_ms`
5), `snapshot_every_fuel`, `cascade_batch` 256 and `notifier`
(`after_commit` or `poll_only`). Its existing rules apply.

### `retry`

`{attempts, initial_delay_ms, max_delay_ms, jitter}` for known-aborted
database work only, never a tool or model call. The pause doubles from the
initial delay to the maximum; jitter draws it from its upper half.

Each operation has one retry owner. An attempt is one complete transaction
from a fresh `BEGIN`: the writer fence and, for an owner commit, its epoch
fence run again, so an attempt fenced out stops the retries. The failed
transaction is rolled back before the pause, and the attempts and pauses
share the operation's deadline (`guards.<role>.operation_deadline_ms`): a
pause that would end past it is not taken, and the contention is the
answer. Contention is SQLSTATE `40001`, `40P01` or `55P03`; a statement
timeout (`57014`) is not.

A durable or store commit whose `COMMIT` answer is lost (the connection
breaks) is not a known rollback. Lash reads the transaction's recorded
outcome (`pg_xact_status`) on a fresh connection: a commit that landed
answers once, one that rolled back runs again under the same policy, and
an outcome it cannot learn before the deadline is answered as unavailable,
never re-applied.

| Policy | Default | Used by |
|---|---|---|
| `store` | 4 attempts, 5–20 ms | Contended store maintenance transactions. |
| `durable` | 4, 5–20 ms | Durable owner and mailbox commits. |
| `wait_resolution` | 3, 5–20 ms | Wait resolutions (`wait.resolve`) and due-wait settlements (`wait.timeout`). |
| `live_replay` | 8, 5–100 ms | Live replay publications and trims. |
| `process_replay` | 8, 5–100 ms | Process replay publications and trims. |

### `signals`

`reconnect` (250 ms to 5 s, jitter): how the durable listener reopens a lost
session. Durable polling stays authoritative meanwhile.

### `live_replay`

Absent by default: no replay pool or listener opens.

| Field | Default |
|---|---|
| `data.schema` | `lash_live_replay` |
| `data.schema_mode` | `verify_only` (`install` runs the published DDL) |
| `data.publish_tick_ms` | 5 (0–1000) |
| `data.publish_concurrency` | 4 (1 to `pool.max_connections`) |
| `data.max_batch_events` | 1024 |
| `data.max_events_per_session` | 2048 |
| `data.max_age_ms` | 120000 |
| `data.max_bytes_per_session` | 8 MiB |
| `data.cleanup_interval_ms`, `cleanup_jitter_ms` | 30000, 10000 |
| `data.cleanup_batch` | 256 |
| `pool` | 7 connections, 5 s acquire, no checkout ping |
| `listener` | its own session, 5 s open |
| `reconnect` | 100 ms to 5 s, jitter |

### `process_replay`

Absent by default: no process replay pool or listener opens. With it,
`host.process_replay` is the `ProcessReplayStore` every replica shares
(`LashCoreBuilder::process_replay_store`): provisional process observation
published on one replica reaches observers on the others. Without it each
OS process keeps its own in-memory store, and a follower converges on the
durable process alone ([observing processes](../observing-processes.md)).

The store has its own tables, incarnation, notification channel
(`<schema>_process_replay`), pool and listener. It shares none of them with
`live_replay`: losing either store's unlogged history gaps only its own
subjects, and neither's traffic takes the other's retention or connections.

| Field | Default |
|---|---|
| `data.schema` | `lash_process_replay` |
| `data.schema_mode` | `verify_only` (`install` runs the published DDL) |
| `data.publish_tick_ms` | 5 (0–1000) |
| `data.publish_concurrency` | 4 (1 to `pool.max_connections`) |
| `data.max_batch_events` | 1024 |
| `data.max_pending_events`, `max_pending_bytes` | 8192, 16 MiB |
| `data.max_events_per_process` | 2048 |
| `data.max_age_ms` | 120000 |
| `data.max_bytes_per_process` | 8 MiB |
| `data.max_processes` | 4096 |
| `data.max_retained_bytes` | 256 MiB (at least `max_bytes_per_process`) |
| `data.reservation_bytes` | 64 KiB (at most `max_bytes_per_process`) |
| `data.cleanup_interval_ms`, `cleanup_jitter_ms` | 30000, 10000 |
| `data.cleanup_batch` | 256 |
| `pool` | 7 connections, 5 s acquire, no checkout ping |
| `listener` | its own session, 5 s open |
| `reconnect` | 100 ms to 5 s, jitter |

The retention values are a provisional preset; no measurement backs them. A
process's window is cut by whichever bound it reaches first, so it lasts about
`min(max_age, max_events / events per second, max_bytes / bytes per second)`:
at 1,000 events a second, 2,048 events are two seconds. Completion does not
shorten a window, and a subscriber does not lengthen it. A subscriber does
keep the window's cursors valid: a process that publishes nothing for longer
than `max_age` (it sleeps, or waits on an approval) is forgotten only when no
replica has a follower on it, so a connected follower sees no gap when it
resumes. Each replica renews that on its cleanup cadence; keep
`cleanup_interval_ms + cleanup_jitter_ms` below `max_age_ms`. `live_replay`
keeps a followed session the same way.

- **Per process.** Events, age (by database time) and encoded bytes. A single
  publication larger than `max_bytes_per_process` is refused and ends the
  process's continuity: its observers get a gap, never a silent loss.
- **Across every replica.** `max_processes` windows and `max_retained_bytes`
  reserved for them. A window reserves its bytes in `reservation_bytes` steps
  and is trimmed to what it reserved, so an event append takes no store-wide
  lock; only a new window or a new step does. When either bound is spent the
  idlest window (the one published to or followed longest ago) is evicted to admit
  another, and its observers get a gap. Cleanup hands unused steps back.
- **Ingress.** A replica holds at most `max_pending_events` events and
  `max_pending_bytes` bytes between `publish` and their transaction; a
  publisher past either bound waits.

### `maintenance`

| Field | Default |
|---|---|
| `preflight_pool` | 2 connections, 5 s acquire |
| `migration_pool` | 2 connections, 30 s acquire |
| `migration_lock_timeout_ms` | 30000 |
| `migration_statement_timeout` | `inherit` |
| `migration_deadline_ms` | none |
| `migration_batch_rows` | 500 |
| `max_sweep_sessions` | 1 attachment sweep session at once |
| `max_schema_sessions` | 1 schema verification session at once |
| `sweep_liveness_probe_timeout_ms` | 500 |
| `process_event_release_page_rows` | 256 |
| `checkpoint_ref_chunk` | 16384 refs/bodies per query |
| `sweep_mint_attempts` | 3 generation collision retries |

### `schema_check` and `deployment`

`schema_check` is `enforce` (default) or `warn_only`. `deployment` declares
what runs beside one process: `processes_per_generation`, `generations` (at
least 2), `other_clients`, `admin_headroom`, `other_host_connections` and
`operator_connections`. With it the connect reads the server's capacity on
its first connection and refuses a budget that does not fit
(`PostgresHostError::Budget`) before any other pool opens. Without it the
check is skipped. Both named presets leave deployment optional; set it to
validate a measured deployment capacity before serving.

## Sizing

The per-process connection count is derived from the document, never entered
by hand (`PostgresHostConfig::connections_per_process`):

```text
P = work + scheduler + critical
  + served_nodes * (1 renewal + 1 listener when notifier = after_commit)
  + live_replay.pool + 1 replay listener         # 0 without live_replay
  + process_replay.pool + 1 replay listener      # 0 without process_replay
  + max_schema_sessions + max_sweep_sessions
  + other_host_connections + operator_connections

peak = processes_per_generation * P * generations + other_clients + admin_headroom
peak <= max_connections
admin_headroom >= superuser_reserved_connections + reserved_connections
```

Worked example: the defaults with one node give 16 + 1 + 3 + 1·(1 + 1) + 1 +
1 = **24**; with live replay, 24 + 7 + 1 = **32** (process replay adds another
7 + 1). Four replicas over two
overlapping generations, ten other clients and ten admin slots declare
`4 · 32 · 2 + 10 + 10 = 276`, so the server needs `max_connections >= 276`.
These are pool maxima: lazy pools open connections only as they are used.

Size the work pool from the workload: `ceil(store transactions per second ·
mean checkout seconds / target utilisation)`, then measure acquire, lock and
commit latency. `max_active` bounds activations, not transactions; widening
the pool to hide acquire waits raises lock contention and server load.

## Poolers

- **Session mode or direct**: the default `direct` topology.
- **Transaction mode** (PgBouncer `pool_mode = transaction`): set `topology:
  "transaction_pool"` and give a session endpoint; the connect refuses the
  topology without one. Transaction pooling keeps transaction advisory locks
  and `NOTIFY` but drops `LISTEN` and session advisory locks, so the
  listeners, schema and sweep sessions, preflight and migration connect
  through the session endpoint. Guards are then installed per transaction
  only, never as session defaults a pooler would hand to other clients; set
  backend defaults for reads outside a transaction. The connect checks that
  both endpoints reach the same catalog.
- Prepared statements need PgBouncer's protocol-level support
  (`max_prepared_statements`); otherwise set `statement_cache_capacity: 0`.
- Validate a pooler's client and backend limits separately: a client cap is
  not a server budget, and direct and session clients still count.
- A host that cannot give a session endpoint sets `node.notifier:
  "poll_only"`, which turns the durable listener off.

## Imported pools

`PostgresStorage::from_pool_set(PostgresPoolSet { work, scheduler, critical,
renewal, session, listener }, config, observer)` takes pools the host built:
their hooks, TLS identity and sizing stay as built. The effective
configuration records each pool's real sizing, the renewal pool must hold
`roles.served_nodes` connections, and every transaction still begins with
its role's guard prelude, so the guards hold whatever the pools' hooks set.
`session` and `listener` must reach a server session.

## Figments migration

Figments' `crates/figments-lash-postgres/src/storage_pool.rs` builds one
SQLx 0.8 pool (`application_name` `lash-runtime-storage`, its sizes and
Session or VerifyDefaults timeouts) and calls `PostgresStorage::from_pool`,
which no longer exists. At its next lash pin:

1. Map `PostgresConnectionConfig` to a `PostgresHostConfig`: `max_connections`,
   `min_connections`, `acquire_timeout`, `idle_timeout` and `max_lifetime` to
   `roles.work`; `statement_timeout` and `lock_timeout` to
   `guards.ordinary`; the prefix `lash-runtime-storage` to
   `connection.application_name_prefix`. Add a nested `durable` /
   `live_replay` / `maintenance` section to its existing settings, keeping
   `LASH_RUNTIME_POSTGRES_STORAGE` overrides as they are.
2. Session timeout policy: connect with `PostgresHost::connect` over
   `PostgresEndpoints::from_url(config.url)`. VerifyDefaults (PgBouncer
   transaction mode): set `topology: "transaction_pool"`, give the session
   endpoint, and keep its backend-default check for reads outside a
   transaction; the guard prelude bounds every transaction.
3. Replace the single-pool metric closure with `host.pool_metrics.snapshot()`,
   one series per role.
4. Declare `deployment` with the product and application pools as
   `other_host_connections` (`apps/lash-runtime` opens them beside storage),
   so the rolling budget counts them.
5. Build the backend with `DurableBackendBuilder::postgres(&host,
   attachments)`.

SQLx 0.9 pools stay outside this boundary: lash pins SQLx 0.8.6.

## What is not configurable

Protocol and identity limits stay fixed: the 7900-byte notification payload
cap, the 16384-element checkpoint bind chunk, advisory lock namespaces, the
sweep generation's three mint attempts, the live replay head's create-or-raise
race loop, and the commit-label routing above.
