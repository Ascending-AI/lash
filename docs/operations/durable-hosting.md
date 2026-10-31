# Hosting lash 1.0 on the durable substrate

Recovery and external-effect guarantees follow [What survives a crash](../../README.md#what-survives-a-crash).

This guide is for host integrators moving to lash 1.0. Lash 1.0 keeps its
own durable state in the lash store and runs no external engine
([ADR 0132](../adr/0132-durability-is-state-first-over-the-lash-store.md)).
The guide covers what a host runs, how it chooses its database topology, the
contracts of the surfaces it implements, and how it operates the result.
[Deploying and upgrading](deploying-and-upgrading.md) covers version
compatibility and rolls. The [Figments migration checklist](figments-migration-1.0.md)
applies this guide to one host, site by site.

Type names below are the ones on main. Paths are facade paths (`lash::...`)
unless they say otherwise.

## 1. What a host runs

### No engine server

A 1.0 host runs no workflow-engine server, registers no handlers and pins no
engine SDK. It does not configure an ingress or admin URL, and it exposes no
handler endpoint.

A host runs **lash nodes**. A node is one host process that serves lash's
runner over a store set:

- **PostgreSQL:** any number of nodes over one database;
- **SQLite:** any number of nodes over one database file, each in a process of
  its own on one machine ([§3](#3-sqlite-is-one-file)).

Work enters through the facade (`LashSession::send`, process starts,
`Completions::resolve`). Each write commits a
mailbox row and wakes the actor that owns the work. Whichever node owns that
actor runs it. A host never drives a turn itself: there is no handler, effect
controller or work driver for it to call.

Hosts may read `core.session(id).durable().await?.owner_node().await?`
or `core.processes().owner_node(&process_id).await?` to learn the committed
actor row's current owner as `Option<lash::durable::NodeId>`. The id is the
host's `runtime_owner.owner_id`; `None` means no node holds the actor,
including when the actor does not exist. These reads never claim, wake or
cache an owner. The answer may be stale immediately. Routing directly to the
named node is optional: correctness never depends on it, because mail sent
through any node still wakes the owner through the store. Lash publishes this
fact; the host owns any routing decision.

### Retrying host delivery with stable keys

Start a process with `ProcessStartRequest::with_host_start_key(key)` when a
host delivery may be retried. The same key and request return the same process
id while that process is retained, including after it ends; reusing the key
for a different request is refused with `PluginError::StartKeyConflict`.
The bytes identify one key across the store set, so the host owns any
partitioning between originators ([ADR 0107](../adr/0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md)).

A host that loses a start acknowledgement retries the same request and key
to recover the id. It must durably record the process id before pruning that
process. Lash keeps no start receipt after pruning: reusing the key then
starts a new process with a new id.

`send(input).id(turn_id)` deduplicates by the id within the session and
validates the original submission digest while the input row or its terminal
tombstone is retained. An input taken by a run retains that evidence until
session deletion; an input withdrawn before admission loses it when host
vacuum removes the tombstone, after which that id can admit new input.

### Build the backend

`lash::durable::DurableBackendBuilder` builds the one `lash::Backend` a
`LashCore` takes:

```rust,ignore
use std::sync::Arc;
use lash::durable::{DurableBackendBuilder, DurableSettings};

let stores: Arc<dyn lash::StoreSet> = Arc::new(lash::postgres::PostgresStoreSet::new(
    &storage,
    attachment_store,
));
let backend = DurableBackendBuilder::new(stores)
    .config(DurableSettings::standard())
    .process_engine(Arc::new(CiEngine))
    .build()?;
// The remaining policy values below are chosen by the host.
let core = lash::LashCore::builder(backend)
    // Example host limits; measure and select them for the deployment.
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
    .execution_budgets(budgets)
    .delta_coalescing(lash::DeltaCoalescing::recommended())
    .data_retention(lash::DataRetention {
        session_revisions: lash::Retention::HeadOnly,
        ..lash::DataRetention::standard()
    })
    // providers, plugins, models, tracing ...
    .build(lash::persistence::LeaseOwnerIdentity::opaque(
        lash::persistence::LeaseOwnerId::new(node_name),
        lash::persistence::LeaseIncarnationId::new(boot_id),
    ))?;
```

### Required choices before serving

The core requires all five policy inputs below. A named preset is a host's
explicit choice of values, not an implicit policy or a measured deployment
recommendation. `build` refuses a missing core choice. The same applies to
session, model, RLM and store choices.

The example selects a 1,024-token action reserve for queued work. The reserve is advisory data for a custom
drain policy; the shipped drain modes do no token arithmetic. Tool and engine
body bounds and park bounds belong to their execution contracts, separately
from the shared model, control and provider budgets.

| Boundary | Required host choices | Current owner |
| --- | --- | --- |
| Core | `commit_budget`, `queued_work_batching`, `execution_budgets`, `delta_coalescing`, `data_retention` on `LashCoreBuilder` | [Builder](../../crates/lash/src/core.rs) and [required-policy validation](../../crates/lash/src/core/runtime_host_config.rs). |
| Session creation | `SessionSpec::model`, `turn_budget`, `max_tool_calls`, `no_progress_budget`; create through `SessionCreation::root(spec)` or `child_of(parent, spec)`. Which tools a session offers is each tool provider's own config (`ToolSelection`), which `spec` states | [Session spec](../../crates/lash-core-execution/src/session_model/mod.rs) and [creation](../../crates/lash/src/session.rs). A child records supplied config; only a fork clones. |
| Recorded model | Wire model, nonzero context window and `CacheRetention` in `LlmProfileMetadata`; choose capability and request defaults for the route | [Model metadata](../../crates/lash-sansio/src/llm_profile.rs). |
| RLM, when installed | `RlmProtocolPluginConfig::builder()` requires `instruction_limit`, `memory_limit` and `channel` before `build()`. The two limits are what each run asks for, and must fit the worker pool's ceiling (`PoolConfig.run_bounds.{charge_limit,memory}`, 50M / 64 MiB standard): a limit past it refuses the plugin when the runtime is assembled, and a session that recorded one refuses to open on that host, with `RunRefusal::AboveCeiling` naming the bound, the limit and the ceiling. Nothing is clamped. `unbounded()` asks for the ceiling | [Typed builder](../../crates/lash-protocol-rlm/src/plugin/config.rs). |
| SQLite | Supply `SqliteSynchronous` to `SqliteStoreSet::open` | [Store open](../../crates/lash-sqlite-store/src/lib.rs) and [§3](#3-sqlite-is-one-file). |
| Tool or process body | Each tool's execution bound, a deferring tool's park bound, each engine step's execution bound and each engine wait's bound | [§6](#6-execution-budgets). These are independent of shared model/control budgets. |

Use [§12](#12-host-prompt-policy) for prompt-plan creation and changes. The
retention decisions below are part of this checklist, including the model's
required cache-retention choice.

### Retention the host states

Lash has no default for what a host keeps. Each of these is required, and
each may be stated as a bound or as its explicit unbounded or keep-everything
value:

| Decision | Where the host states it | Choices |
| --- | --- | --- |
| Attachment puts, reads, upload expiry and retained output | `LashCoreBuilder::data_retention`, `DataRetention::attachments` | A put bound or `None` for unbounded; read budgets; an upload expiry; the inline limit and witness size of retained output. |
| Session revisions | `DataRetention::session_revisions` | `Retention::UntilGc`, `LastTurns(n)` or `HeadOnly`. Recorded with each session the core creates; `LashSession::set_retention` changes one session's. |
| Live replay and process replay | `DataRetention::live_replay`, `DataRetention::process_replay` | Event, age, subject and byte bounds, stated apart for sessions and processes. A host store installed with `live_replay_store` or `process_replay_store` carries its own. |
| Prompt-cache retention | `LlmProfileMetadata::builder(..).cache_retention(..)`, per model | `CacheRetention::None` (off), `Short` (the provider's default lifetime) or `Long` (the extended lifetime). Recorded with each session's model binding. |
| SQLite durability | `SqliteStoreSet::open(path, synchronous)` | `SqliteSynchronous::Full` or `Normal` ([§3](#3-sqlite-is-one-file)); `Off` only for stores whose loss is acceptable. |

`DataRetention::standard()` is the named preset a host may choose; its rustdoc
lists every value, and no measurement backs them. A core built without
`data_retention` refuses with `EmbedError::MissingDataRetention`, and a model
whose metadata states no cache retention refuses with
`LlmProfileLimitsError::MissingCacheRetention`.

`build` validates the settings and refuses with `lash::durable::DurableBuildError`:

| Refusal | Cause |
| --- | --- |
| `InvalidConfig(DurableConfigError)` | A `DurableSettings` field breaks a rule ([§5](#durableconfig)). |
| `DuplicateEngine { kind }` | Two process engines declare one kind. |
| `DuplicateProvider { projection }` | Two projection providers answer one type, or a host provider claims the lash-provided `history` type. |

`projection_provider` exists with the facade's `rlm` feature.

### Node identity

A `LashCore` runs one node from the moment it is built on a runtime (or from
the first session it opens, when it was built outside one). The node serves
under the `owner_id` of the `lash::persistence::LeaseOwnerIdentity` the
builder's `build` takes: a stable name the host chooses, such as the pod
name, the host name or a configured value. Every start of the process is a
new boot. A new boot under a name that is still registered fences the old
boot: the old boot's commits fail with `OwnershipLost`. Never give two live
nodes the same name. `LashCoreBuilder::serve_sessions(false)` builds a core
that runs no node; it still sends, reads and administers, and its sessions'
turns run on the deployment's other nodes.

A node decodes the format sets of its backend's build (`Backend::formats`)
and claims only actors whose state is written in one of them
([§8](#format-compatibility)).

### Heartbeat, self-stop and reap

Each node keeps one heartbeat row. `DurableSettings::lease`
(`lash::durable::LeaseSettings`) sets its timings:

| Field | Default | Meaning |
| --- | --- | --- |
| `ttl` | 15 s | How long one heartbeat keeps the lease alive. |
| `heartbeat_every` | 3 s | How often the node renews. Must be shorter than `self_stop_after`. |
| `self_stop_after` | 10 s | With no successful renewal for this long, the node stops itself: it drops every actor and cancels every body. Must be shorter than `ttl`, so a partitioned node stops before anyone may reap it. |
| `reap_every` | 2 s | How often the node reaps dead nodes. |
| `claim_poll` | 250 ms | The idle claim interval's ceiling, and the longest a lost wake can delay work. |
| `claim_backoff` | 25 ms | The claim interval's floor after a claim that took work. |
| `startup` | 2 s | How long registration and the listener's start may take together, from the registration attempt. A node not in place by then fails to start; renewal already runs while the listener opens. |
| `shutdown` | 2 s | How long a stopping node waits for its lease's release. A release that does not answer in time is left to the lease's expiry. |

The reaper deletes an expired node row and, in the same statement, releases
its actors with an **epoch bump**. Every owner transaction begins by reading
its actor's epoch under lock. A node that lost its actors therefore cannot
commit for them, even while it still runs: its write fails `OwnershipLost`,
rolls back, and the node drops the actor. Lease expiry is never checked at
commit time.

With the `AfterCommit` notifier, a crashed node is also detected through its
listener's liveness lock: a session advisory lock on PostgreSQL
([§2](#2-postgresql-topology)), a file lock beside the database on SQLite
([§3](#3-sqlite-is-one-file)). The other nodes reap it within about one claim
poll instead of waiting out the lease. The failover runbook measured 257 ms
with the lock and 15 963 ms without it on PostgreSQL, and 163 ms and 15 913 ms
on SQLite
([failover bound](../../runbooks/lash-postgres-workers/README.md#failover-bound-l8-fig-5178)).

### Clean shutdown

`LashCore::shutdown` stops the core's artifact-cleanup pass, stops its node,
then shuts down its plugin factories. A node stops with one of the runner's
reasons (`lash::durable::runner::Stopped`):

- `Requested`: the host asked. The node releases its actors, and another node
  claims them within a claim poll. The runbook measured a 7 ms handover.
- `LeaseLost`: another node reaped this one, or a newer boot of its name
  replaced it.
- `Unrenewed`: renewals failed for `self_stop_after`, so it stopped before
  anyone could reap it.
- `Drained`: it drained by release ([below](#drain-by-release)).

A core's node that stops `Unrenewed` while the host neither shut the core
down nor drained it (a stalled process: a starved host, a long pause) serves
again at once as a new boot of the same owner. The new boot's registration
fences the old one and releases its actors, so the node resumes every
session it held, and its listener holds the liveness lock again (FIG-6887).
A start that a transient store failure refused is retried the same way.

`LashCore::node_stopped` waits until the core's node stops for good and
answers which reason it was (`None` for a core built with
`serve_sessions(false)`). A node that stopped `LeaseLost`, or whose store
refused it for good, stopped on its own: the core still admits work, which
other nodes serve, but this process serves none until it restarts as a new
boot. A host that wants every process serving watches it and exits on such a
stop, so its supervisor restarts it. The
[facade-host failover runbook](../../runbooks/lash-facade-failover/README.md)
does this.

Call `shutdown` on SIGTERM, after the host stops intake and before the
platform's kill deadline. A node killed without stopping costs one reap: one
claim poll with the liveness lock, or `ttl` + `reap_every` without it. No
acknowledged work is lost either way. A node served directly with
`node::serve(&backend, serve, stop)` stops when its `stop` future completes,
for the same reasons.

The lease renews on a task of its own, one heartbeat at a time, from the
moment the node registers. A claim, reap, hand-back or liveness probe that
waits on a busy pool or a held lock never delays a renewal. Each renewal moves
the self-stop deadline to `self_stop_after` past the moment its heartbeat was
sent, so a late answer never extends serving past the lease it renewed. A
heartbeat that hangs still stops the node at `self_stop_after`, and the
serving loop's own store calls race the host's stop and the renewal's end, so
the node's activations stop with it. Bound the store's connect and statement
timeouts anyway, so a hung call ends.

### Drain by release

A release that changes a durable format retires the old build by draining it
([ADR 0106 §1](../adr/0106-durable-formats-upgrade-by-migration-or-drain.md#1-long-running-work-and-drain-by-release)).
`LashCore::drain()` drains the core's node and waits until it stops:

1. the node records itself draining and claims nothing more;
2. each session it owns stops at its next committed phase, before its next
   model call or code cell; a turn's model call or tool round that is
   already running finishes and commits first;
3. each one is released `ready` under `drain.release`, and when none is left
   the node releases its lease.

Nodes of the next build claim the released actors whose formats they decode
and resume each from its committed rows: effects with committed outcomes
do not re-execute, while uncommitted work follows the crash contract,
and a `SendHandle` taken on the old build still answers when the turn
finishes elsewhere. `drain` answers a `lash::NodeDrainReport` naming the
`sessions` and `processes` it released, in release order. It refuses with
`lash::NodeDrainError`:

| Refusal | Cause |
| --- | --- |
| `NotServing` | The core runs no node: it serves no sessions, or it shut down first. |
| `Stopped(Stopped)` | The node stopped for another reason before it drained, such as a lost lease or a `shutdown` during the drain. What it still owned is claimed by other nodes as after a crash. |
| `Store(DurableError)` | The store refused the node's registration or its release. |

A drained core never starts a node again, and draining it again answers the
same report. It still admits work: a send writes its mail and wakes its
session, and the next build's nodes run it. Call `shutdown` once the old
build's intake has moved. A node served directly drains when the host starts
the `Drain` its `NodeServe` holds, and `serve` returns `Stopped::Drained`.

### Host-scheduled maintenance

Serving nodes do not schedule attachment reclamation or terminal-process
pruning. The host installs those jobs and owns their cadence and retention
policy. Logical database cleanup and deletion of physical attachment bytes
are separate operations:

| Maintenance | Caller and lever | Scheduling and retention owner |
| --- | --- | --- |
| Attachment referrer cleanup in the database | Lash's store transactions retire edges as their owners and retained records are released; reclamation reconciles abandoned write intents against durable owner state. | The host chooses record retention and deletion. Removing referrers makes bytes eligible; it does not delete blobs. Age alone does not prove a turn or retained process dead. |
| Physical attachment reclamation | A host job calls `lash::persistence::reclaim_unreferenced_attachments(&root_set, attachment_store, policy)`. The root authority is the deployment's `PostgresStore` or `SqliteStore`, implementing `AttachmentRootSet`, paired with that deployment's blob backend. | The host chooses the schedule and `AttachmentReclamationPolicy`: `grace_period_ms`, `EmptyRootSetPolicy` and optionally the delete-attempt limit. |
| Terminal-process pruning | A host job calls `core.processes().prune(cutoff_epoch_ms, filter, watermark)`, returning `ProcessPruneReport`. | The host chooses cutoff and selection, and supplies `ProjectionWatermark::UpTo(acknowledged_cursor)` or explicitly `NoProjector`. Retain rows through every outstanding delivery/reconciliation horizon and still-replayable await; persist each start-key binding before pruning. A later await receives `ProcessNoLongerRetained`. |

The attachment backend's **namespace must be exclusive to the deployment
whose root authority the sweep reads**. For S3, that namespace is the
configured bucket plus prefix; deployments with distinct exclusive prefixes
may share one bucket. For a filesystem backend it is the configured directory.
Every blob the backend enumerates or deletes must belong to that authority.
An unrelated nonempty root set does not protect another deployment's data;
the empty-root guard is not an ownership check.

Choose grace to cover both post-terminal retention of uncommitted intents
and freshness of recently put blobs. The sweep checks durable owner state
before forgetting an intent and rechecks blob freshness with `head` before
deleting; grace is host retention policy, not a maximum run or replay time.
Choose `EmptyRootSetPolicy::Refuse` unless deleting all unreferenced,
deletion-eligible blobs in this namespace is intended, in which case state
`AuthorizeDeleteAll` explicitly. An unauthorized destructive empty-root
sweep returns `MaintenanceRefusal::EmptyRootSetUnauthorized` with a partial
report.

The built-in database authorities fence deletion through condemnation,
writer-intent and delete-arming CAS transitions. Each sweep first adopts and
finishes condemnations left by provably dead passes, then considers new
candidates. Contended or restoring digests are deferred. Custom authorities
must implement every root-port fence transition against the writer's durable
authority. Unsupported operations return typed `UnsupportedStoreOperation`
errors before granting deletion; there is no best-effort fallback. Investigate
any `deleted_while_referenced` report as a fence invariant violation.

Inspect the report even on `Ok`: `MaintenanceReport::sweep()` classifies
`failed_ids`, `condemn_deferred_ids` or `stalled_ids` as
`MaintenanceSweep::Incomplete`. Later host sweeps retry failed and stalled
deletes under backoff; `AttachmentRootSet::list_condemnations` reports each
reason, attempt count and last error, and `adopted_count` records adopted
work. `Err(AttachmentReclamationFailure)` means a refusal or sweep-level
failure stopped the pass and carries any partial report. Surface these
results to operators instead of treating a completed call as a complete
cleanup.

See [reclamation policy, fences and reports](../../crates/lash-core-store/src/attachments.rs)
and [process pruning](../../crates/lash/src/process_admin.rs).

## 2. PostgreSQL topology

PostgreSQL 17 and 18 are supported for lash 1.0; 18 is primary. Lash's CI
runs 18, and every release also passes its PostgreSQL suites on 17. Nothing
older than 17 is supported.

### Connections

One `PostgresHostConfig` sizes and guards every connection
([`postgres.md`](postgres.md) is the reference). Each process opens:

- the work pool (`roles.work`, 16) for store calls and ordinary durable
  commits, behind `roles.max_store_operations` admission;
- the scheduler pool (`roles.scheduler`, 1) for claims, adoption, liveness
  and drain marks, and the critical pool (`roles.critical`, 3) for reaps,
  releases, hand-backs, cancels and terminals, so a burst of ordinary commits
  can neither starve a claim nor hold back an ending;
- per served node, a renewal connection for its lease's registration and
  heartbeat, and one listener session, which receives wake hints (`LISTEN`)
  and holds the node's liveness lock, a session advisory lock.

One default node costs 24 server connections; declare `deployment` and the
connect checks the whole rolling budget against the server before any other
pool opens. Build the durable backend with
`DurableBackendBuilder::postgres(&host, attachments)`, which takes the
durable settings from the host configuration's `node` section.

**`LISTEN` and advisory locks need session-mode connections.** Connect the
listener directly or through a pooler in session mode. A transaction-mode
pooler silently drops both the subscription and the lock. Hints then never
arrive, which costs only latency, and crashes are detected by lease expiry
alone. A host that must use a transaction-mode pooler sets
`Notifier::PollOnly`, which turns the listener off.

Set `tcp_keepalives_idle`, `tcp_keepalives_interval` and
`tcp_keepalives_count` on the server so a partitioned node's session ends in
bounded time.

### Durability across a failover

`Once` means a tool body starts at most once. Lash enforces it with a row: a
started row commits before the body runs, and recovery finds it and records
`Interrupted` instead of starting again. The guarantee is only as good as
that row's survival.

A crash after a model call's receipt commits costs no extra model request:
the turn derives the response again from the receipt. A crash between the
provider's answer and its receipt commit can cost one extra model request
with a different response, while committed tool work with committed outcomes
remains intact ([ADR 0142](../adr/0142-a-cells-first-save-carries-its-turns-model-response.md)).

**`Once` holds across a database failover only if acknowledged commits
survive promotion.** The host chooses its topology. Lash runs no startup check
and offers no mode switch
([ADR 0132 §13](../adr/0132-durability-is-state-first-over-the-lash-store.md#13-the-storage-durability-assumption)).

- **Synchronous replication** (`synchronous_commit = on` or `remote_apply`,
  with the promoted standby in `synchronous_standby_names`). An acknowledged
  commit is on the standby before the primary answers. A promotion keeps every
  started row, outcome and epoch bump, so `Once` holds.
- **Asynchronous replication.** The primary answers before the standby has the
  commit. A promotion can lose the last acknowledged transactions. Lost
  started rows can let a `Once` body run a second time; lost outcomes are
  recorded `Interrupted`; lost epoch bumps can let a fenced node's writes
  through. The guarantee becomes "at most once, except on loss of
  acknowledged commits".
- **One primary with no failover.** `Once` holds while the primary lives.
  Restoring it from a backup or a point-in-time recovery is a loss of
  acknowledged commits, with the same consequence as an asynchronous
  promotion.

Recipients that deduplicate on lash's stable call identity
([§4](#recipients-deduplicate)) stay safe under every topology. That is the
reason to make them do so.

Durable instants come from the database clock on PostgreSQL, so node clock
skew does not move deadlines.

### External effects

A host-owned tool that writes to a payments API or ticket tracker can keep
its own durable ledger. Before the external write, the tool body claims a
row keyed by `AttemptContext::call_id()` and records its intended effect as
`pending`. It uses that id as the outside system's idempotency key where
supported, then marks the row `applied` after confirming the write.

A tool result check receives the same id as `ToolHookContext::call_id`.
It only observes the reported outcome; it neither performs the external
write nor proves that it landed. A retry can report another outcome under
the same id, so observations must deduplicate on that id.

On restart, the host reconciles pending rows against the outside system.
It marks effects applied only when reconciliation confirms them and
compensates only effects that reconciliation proves occurred. If undo must
resume after a crash, the host durably records compensation progress.
There is no atomic transaction between the host's row and the outside
write; an unconfirmed effect remains pending for reconciliation.

Lash runs no part of this ledger, reconciliation or compensation. This is
a host pattern with no Lash law behind it. Lash supplies the
[call identity](../adr/0117-lash-names-every-tool-call.md), not proof of an
external effect.

## 3. SQLite is one file

A SQLite deployment is **one database file**. It holds the session catalog,
the process registry and the durability core, so one
transaction commits rows of every family
([Deploying and upgrading](deploying-and-upgrading.md#choose-the-deployment-shape)).

- Open it with `lash::sqlite::SqliteStoreSet::open(path, synchronous)`. The
  host states the `synchronous` mode; there is no default.
  `SqliteSynchronous::Full` syncs the log at every commit, so a commit that
  answered survives a power loss. `SqliteSynchronous::Normal` syncs at
  checkpoints: the database stays consistent through a power loss, but the
  commits since the last checkpoint can be rolled back, so a turn lash
  answered as committed may be gone after the machine restarts. A process
  crash alone loses nothing in either mode. Tests use
  `SqliteStoreSet::memory()`.
- **Several processes on one machine.** Any number of lash processes on one
  machine may open one file, each serving a node of its own, through the node
  interface PostgreSQL uses: leases, epoch fencing, takeover, wake hints
  between nodes, and crash detection by liveness lock. Every write takes
  SQLite's write lock (`BEGIN IMMEDIATE`), so writers in every process
  serialize and any node may claim any work. A writer that finds the lock
  taken waits up to the connection's busy timeout (15 s by default).
- **One owner per process.** Each process runs its own node boot under an
  owner of its own (`LeaseOwnerIdentity::owner_id`, [§1](#node-identity)). A
  second boot of an owner that is still registered fences the first by
  design: the first boot's commits fail `OwnershipLost` and it stops. Two
  processes that share an owner keep stopping each other. lash-cli uses one
  owner per home directory, so two lash-cli processes over one home fence each
  other; that is lash-cli's to fix.
- **Wakes and liveness.** With `Notifier::AfterCommit`, a node publishes
  what its commits woke as rows in the file, after those commits. Each node's
  listener polls the file's data version every 25 ms on a thread of its own,
  and reads the rows addressed to it when the version moves. A wake between
  processes costs about one poll: about 25 ms at the median and about 100 ms
  at worst over twenty measured mails. A wake between two nodes of one process
  waits for no poll. An idle listener costs about 0.3% of one core. The
  listener also holds its boot's liveness lock, an `flock` on a file in
  `<database>-liveness/`. The kernel drops it when the process dies, however
  it dies, so the other nodes reap a killed node within about one claim poll:
  70 ms to 210 ms after `SIGKILL` in the measured runs. Wakes and liveness
  only cut latency: a lost wake costs a poll, and the epoch is the only
  fence.
- **Local disk only.** The database, its `-wal` and `-shm` files and its
  `-liveness` directory must be on a local filesystem. A network filesystem
  breaks SQLite's locking and `flock`, and is not supported. Use PostgreSQL
  for nodes on more than one machine.
- **Scale.** One writer commits at a time, for every process on the file, so
  SQLite scales less far than PostgreSQL.
- A path that is a directory in the retired three-file layout is refused
  `retired_sqlite_layout`. Formats reset at 1.0; nothing migrates it.
- Durable instants come from the injected clock: in production the machine's
  clock, which every process on the file shares.
- A memory store set lives in one process and has no node wakes. Its nodes
  find each other's work through the claim poll and the mail scan, and a
  dead node is reaped when its lease lapses.

## 4. Host process engines

A host `ProcessEngine` (`lash::plugins::ProcessEngine`) is an explicit state
machine
([ADR 0132 §10](../adr/0132-durability-is-state-first-over-the-lash-store.md#10-host-process-engines-are-state-machines)).
Lash calls `advance(state, event)` with the process's committed state and one
event. The engine answers its next state and one action. The new state and
the action's admission commit in one `process.advance` transaction before the
action runs.

### Running through the facade

Register a host engine with `lash::durable::DurableBackendBuilder::process_engine`
before building the backend, as in [§1](#build-the-backend). A plugin factory
can contribute `lash::plugins::ProcessEngineRegistration` values through
`process_engine_contributions`; pair an engine's own step bodies with
`ProcessEngineRegistration::with_engine_steps` (`lash::plugins::EngineSteps`).
The core installs the contributed engines alongside the backend's engines.

A serving `LashCore` runs both session and process actors on its node. Its
production process worker dispatches catalog tool steps through the tool
execution path and engine steps through the registered `EngineSteps`.
A `SessionTurn` process submits its turn to its child session; the node's
session actor drives that turn. The RLM plugin contributes the Lash VM
engine and its VM step bodies through the same registration path.

Hosts start processes through
`core.processes().start(operation_id, request).await` or send input
through `LashSession::send`; they observe the resulting receipts, handles
and lifecycle facts. The core's node advances the processes. Hosts need no
separate runner or process activation. With `serve_sessions(false)`, the
core runs no node, and other serving cores in the deployment run both its
sessions and its processes.

### The contract

- **`advance` is effect-free and deterministic.** It is synchronous. The same
  `EngineState` and `EngineEvent` give the same answer. It calls no service,
  reads no clock and writes nothing. If the commit after it fails, lash calls
  `advance` again with the same state and event; that is recomputation, not
  replay.
- **There is no `run`.** There is no opaque run method, no `await_terminal`
  and no re-entry from the top. No trait method has a default body.
- **Effects are actions.** `EngineAction` has seven variants:

  | Action | Lash does | `advance` next receives |
  | --- | --- | --- |
  | `Steps(Vec<StepRequest>)` | Admits every step (started rows) in the transaction, then runs them. Non-empty. | One `StepSettled { step, outcome }` per step |
  | `PinKey { name, bound }` | Mints a host-resolvable wait named `name` and its key. | `KeyPinned { name, key }` at once |
  | `AwaitExternal { name }` | Waits on a key pinned earlier. | `ExternalResolved { name, resolution }` or `ExternalTimedOut { name }` |
  | `AwaitProcess { process, bound }` | Waits on another process's terminal. | `ProcessEnded { process, outcome }` or `ProcessWaitTimedOut { process }` |
  | `Sleep { until }` | Sets a durable due time. | `Woke` |
  | `Idle` | Nothing, with no deadline. | The next mailbox event |
  | `Terminal(ProcessOutcome)` | Ends the process. | Nothing |

  `Cancelled { origin, grace_until }` can arrive whenever the process is waiting. `Started { payload }` is the first event.
- **Steps.** `StepRequest::Tool { step, tool, input }` names a catalog tool.
  Its declaration's `ExecutionPolicy` (`Once` or `Repeatable`) and a limit
  set by the host are pinned at admission. Its body runs through the
  admitted-execution primitive: a started row first, then the body. A crash
  then gives the usual result. A started `Once` step without an outcome
  settles `Interrupted` and never runs again. A `Repeatable` step runs again
  at the same ordinal. `StepRequest::Engine { step, kind, input }` runs one of
  the engine's own bodies, declared at registration with
  `ProcessEngineRegistration::with_engine_steps` (the `EngineSteps` trait).
  An engine step always runs `Repeatable`, so its body must be a pure
  recomputation from its input. A step settles as a `SettledOutput`: its
  `Completed` and `Failed` variants carry the material's payload, checked
  against the digest the outcome names, and the stopped ones carry none. An
  `Interrupted`, `TimedOut` or `Cancelled` step reads as
  `SettledOutput::stopped_answer`, the answer a turn gives the same call.
  For parallel work, put several requests in one `Steps`; a single step is a
  one-element vector.
- **Pin first.** Mint a key with `PinKey` before any step hands it out. The key
  exists once `KeyPinned` arrives, so a resolution that comes back before the
  process awaits still finds its wait row.
- <a id="recipients-deduplicate"></a>**Recipients deduplicate on the stable
  action identity.** A step's identity is `(process, step, ordinal)`, and its
  `ToolCallId` derives from it. The `ToolCallId` stays the same when a
  `Repeatable` step runs again, and a pinned key names one wait for its whole
  life. An external system that receives work keys it on the `ToolCallId` or
  the key, so a re-sent request is recognized.
- **Wait bounds are host-set.** `PinKey` and `AwaitProcess` take an explicit `bound: ParkBound`
  the host declares, with no wait default or ceiling. The configured execution
  budgets govern the running work. An `AwaitProcess` races the awaiter's own
  cancel mail, so a cycle of processes awaiting each other stays cancellable.
- **Refused actions end the process.** A `Steps` with no request, a step name
  already in flight, an `AwaitExternal` for a key never pinned or a refused
  step admission ends the process with a `process_action_refused` failure.
- **Cancel is cooperative, then forced.** A running or waiting process
  receives `Cancelled { origin, grace_until }` once. `grace_until` is the
  committed cancel request's time plus the engine's `cancel_grace()`, recorded
  at creation. Within the grace the engine may answer best-effort `Steps`
  (for example a remote cancel) or a `Terminal`. At `grace_until` lash commits
  a forced `Cancelled` terminal without calling `advance` again and drops the
  steps. Lash ends the process; it never claims that the remote work stopped.
- **A parked or waiting process ends engine-free when it must.** A parked
  process, one whose state this node cannot decode, and one that never started
  end without running their engine: the claimer commits the terminal from
  registry state.
- **Format.** `state_format()` names the encoding (`EngineStateFormat { kind,
  version }`). Bump `version` when the encoding changes; a node whose engine
  does not read a process's format parks it `UndecodableState` instead of
  guessing.

The other trait methods answer registration questions: `kind`,
`program_identity`, `check_settings` and `default_settings` (the engine
settings a start states, recorded on the process row in its launch and handed
back to engine steps as `EngineStepRun::engine_settings`), `start_artifacts`,
`end_artifact_referrer`, `acquire_engine_artifact`, `check_args` and `resolve`.
`check_args` checks supplied arguments against an authoritative signature;
engines without a checkable signature return `ArgsMismatch::UnsupportedSignature`.
Hosts invoke it through
`core.process_definitions().check_args(&definition, &args, mode).await`, with
`ArgsMode::Partial` or `ArgsMode::Complete`. Partial checks every supplied value
and rejects undeclared names; Complete also requires every declared argument.
The facade reads the retained definition, verifies its signature claim, and
checks arguments without starting a process or acquiring a lasting pin.
The outer `lash::Result` reports operational errors, including the typed
`PluginError::StoreUnavailable` store fault. `Ok(Err(ArgsMismatch))` is an
argument or definition judgment with a stable local `code()`; `Ok(Ok(()))`
accepts the arguments. An outage never reads as invalid input.
Host-artifact errors expose `EmbedError::artifact_refusal()` and the refusal's
stable `code()` (`pin_released`, `definition_missing`, `artifact_missing`,
`conflict`, `definition_refused`). Operational failures return `None` and keep
their typed cause and retryability. These codes do not choose HTTP statuses.

Database failures retain their native code in
`StoreError::StorageFailure { backend, message, code }` and the serialized
`StoreFault::StorageFailure`. The optional `lash::persistence::StorageCode`
is backend-specific: PostgreSQL carries its five-character SQLSTATE and
its two-character class; SQLite carries its extended result code. Hosts may
classify PostgreSQL class `40` as transaction rollback, `08` as a connection
exception and `25` as invalid transaction state. Use these fields instead of
parsing `message`. `None` means no database code was supplied, including
pool, I/O and decode failures. These diagnostics do not change Lash's runtime
error codes, contention classification or retry policy.

### Worked example: a 40-minute CI suite

A tool `run_full_suite` runs a CI suite that takes about 40 minutes. That is
far longer than a turn should hold a tool body, so it is declared as a process
tool that starts a `ci` process. The process submits the job, then waits for CI to call back with
the key.

```text
TURN    run_full_suite admitted -> its body starts process P (kind "ci") -> durable wait on P
P       Started            -> PinKey { name: "done", bound: Within(45 min) }
        KeyPinned { key }  -> Steps([Tool { step: "submit", tool: "ci.submit",
                                            input: { suite, key } }])     ci.submit is Once
        StepSettled(Completed) -> AwaitExternal { name: "done" }           P releases as waiting
CI      40 min later: Completions::resolve(key, Resolution::Ok(report)), retried until answered
P       ExternalResolved   -> Terminal(success: report)
TURN    the wait on P resolves -> model -> commit
```

While P awaits `done` it reads as waiting: its lifecycle log records
`process.waiting` with `WaitKind::Key { name }`, without the bearer key, and
`process.resumed` when the wait ends (`process.wait_cancelled` when P's cancel
cuts it short). A host that lost the key, or restarted
before CI called back, reads it again with `Completions::pinned_keys(P)`.

```rust,ignore
use lash::tools::ParkBound;
use std::time::Duration;
use lash::plugins::{
    EngineAction, EngineEvent, EngineState, EngineStateFormat, KeyName,
    ProcessEngine, ProcessInfraError, StepName, StepRequest,
};
use lash::process::ProcessOutcome;
use lash::tools::{ToolCallOutput, ToolId};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
enum Ci {
    #[default]
    New,
    Pinning { suite: String },
    Submitting { key: String },
    Waiting,
    Cancelling,
}

struct CiEngine;

impl CiEngine {
    fn state(&self, ci: &Ci) -> Result<EngineState, ProcessInfraError> {
        Ok(EngineState { format: self.state_format(), bytes: serde_json::to_vec(ci).map_err(infra)? })
    }
}

#[async_trait::async_trait]
impl ProcessEngine for CiEngine {
    fn kind(&self) -> &'static str { "ci" }
    fn state_format(&self) -> EngineStateFormat {
        EngineStateFormat { kind: "ci".into(), version: 1 }
    }
    fn cancel_grace(&self) -> Duration { Duration::from_secs(30) }

    fn advance(
        &self,
        state: EngineState,
        event: EngineEvent,
    ) -> Result<(EngineState, EngineAction), ProcessInfraError> {
        let ci: Ci = if state.bytes.is_empty() {
            Ci::New
        } else {
            serde_json::from_slice(&state.bytes).map_err(infra)?
        };
        let done = || KeyName("done".into());
        // `infra`, `is_completed`, `failed`, `from_resolution` and
        // `cancelled` are small local helpers, elided here.
        let (next, action) = match (ci, event) {
            (Ci::New, EngineEvent::Started { payload }) => (
                Ci::Pinning { suite: payload["suite"].as_str().unwrap_or_default().into() },
                // Pin first: the key exists before any step hands it out.
                EngineAction::PinKey {
                    name: done(),
                    bound: ParkBound::Within(Duration::from_secs(45 * 60)),
                },
            ),
            (Ci::Pinning { suite }, EngineEvent::KeyPinned { key, .. }) => (
                Ci::Submitting { key: key.as_str().into() },
                // ci.submit is a Once catalog tool. CI deduplicates on the
                // key and on the step's ToolCallId.
                EngineAction::Steps(vec![StepRequest::Tool {
                    step: StepName("submit".into()),
                    tool: ToolId::new("ci.submit"),
                    input: serde_json::json!({ "suite": suite, "key": key.as_str() }),
                }]),
            ),
            (Ci::Submitting { .. }, EngineEvent::StepSettled { outcome, .. }) => {
                if is_completed(&outcome) {
                    (Ci::Waiting, EngineAction::AwaitExternal { name: done() })
                } else {
                    // Interrupted (a crash during submit), failed or timed
                    // out: report it; never resubmit a Once step.
                    (Ci::Waiting, failed("the CI submission did not complete; nothing was relaunched"))
                }
            }
            (Ci::Waiting, EngineEvent::ExternalResolved { resolution, .. }) => {
                (Ci::Waiting, from_resolution(resolution))
            }
            (Ci::Waiting, EngineEvent::ExternalTimedOut { .. }) => {
                (Ci::Waiting, failed("CI did not answer within 45 minutes; the job may still be running"))
            }
            (_, EngineEvent::Cancelled { .. }) => (
                Ci::Cancelling,
                // Best effort within cancel_grace; lash forces the terminal
                // at grace_until whatever happens.
                EngineAction::Steps(vec![StepRequest::Tool {
                    step: StepName("cancel".into()),
                    tool: ToolId::new("ci.cancel"),
                    input: serde_json::json!({}),
                }]),
            ),
            (Ci::Cancelling, EngineEvent::StepSettled { .. }) => (Ci::Cancelling, cancelled()),
            (ci, _) => (ci, EngineAction::Idle),
        };
        Ok((self.state(&next)?, action))
    }

    // program_identity, check_settings, default_settings, start_artifacts,
    // end_artifact_referrer, acquire_engine_artifact and resolve answer
    // "none" for this engine; see crates/lash-postgres-workers/src/process.rs.
    // check_args explicitly returns ArgsMismatch::UnsupportedSignature.
}
```

Each crash case reads from committed state:

- **A crash before the submit's admission commits:** nothing ran. The next
  pass calls `advance` again from the last committed state and event, and
  admits the step once.
- **A crash during the submit:** the started `Once` step settles
  `Interrupted`. The engine reports a failure and resubmits nothing. CI may
  have received the job; it ran under the same key, so a late resolution
  answers `Revoked` once the process has ended.
- **A crash while waiting:** the process holds nothing. The wait row and its
  deadline survive, and any node resumes it when CI resolves or at the
  deadline.
- **CI never answers:** at 45 minutes `ExternalTimedOut` arrives and the
  process fails, saying the job may still be running.

Lash's own lash_vm engine follows the same contract: its `advance` answers
`Steps([vm_run])`, and the VM runs only inside that engine step. A host does
not register it: the RLM protocol plugin factory contributes it, with a
`LashVmRunSettingsRecorder` that records each process's surface at creation.

## 5. Completion keys

A host-resolvable wait has a completion key: its wait id, 128 random bits from
the operating system's CSPRNG, spelled as 32 lowercase hex digits
(`lash::durable::PinnedKey`). The key carries no scope or kind, and lash keeps
no completion secret: there is nothing to provision, rotate or share between
nodes.

The key is a bearer capability: whoever holds it can resolve its wait. Who may
finish a pending wait is authorization, and the host owns it. Authenticate and
authorize the caller (your API authentication, your webhook signatures) before
resolving on its behalf, hand a key only to callers you have authorized, and
keep keys out of logs and URLs others can read.

### Resolving

`LashCore::completions()` returns `lash::admin::Completions`:

- `parked(owner)` lists a session's or process's pending admitted tool calls.
  `lash::admin::CallOwner` is `Session(SessionId)` or `Process(ProcessId)`;
  each `ParkedCall` carries `key`, `owner`, `call_id`, `tool_id` and `deadline`.
  The listing reads the owner's pending wait rows, which record the call and
  tool when the wait is pinned, so its cost follows what is parked and not
  the runs the owner retains. It is a snapshot, so a concurrent resolver may
  settle a returned key before the host uses it.
- `pinned_keys(process)` lists the pending keys a process's engine pinned
  with `PinKey`. Each `lash::admin::PinnedEngineKey` carries `key`, `process`,
  the `name` the engine pinned it under and `deadline`. It reads the same
  wait rows, so a host finds a key on any node and after a restart or a
  handover; it never needs the `KeyPinned` event, which the engine sees once.
- `resolve(key, resolution)` resolves the key's wait, first writer wins, and
  answers `lash::durable::ResolveAnswer`:

| Answer | Meaning |
| --- | --- |
| `Resolved` | This resolution won; the owner was woken. |
| `AlreadyResolved` | An earlier resolution with the same digest won. Safe to treat as success. |
| `Conflict` | An earlier resolution with a different digest won. |
| `ReservedKind` | The key names a kind a host may not resolve. Nothing was written. |
| `Unknown` | No wait has this key. Nothing was written. |
| `Revoked` | The key's wait was revoked, or timed out, first. Nothing was written. |

Hosts resolve only the `tool_completion` and `engine_key` kinds. Turn
cancellation, process terminals, timers and child-session ends have their own
admission paths, and a host resolution of them answers `ReservedKind`. Lash
applies no authorization of its own: authenticate and authorize the caller
before resolving on its behalf
([ADR 0014](../adr/0014-operational-policy-stays-with-the-host.md)).
A webhook that receives callbacks retries until it gets an answer and treats
`AlreadyResolved` as done.

## 6. Execution budgets

`lash::ExecutionBudgets` supplies the shared model and control-phase bounds.
Tool bodies and parks, and process step bodies and waits, carry their own
host declarations below. The shared budget is the host's spend decision and has no default: state it with
`LashCoreBuilder::execution_budgets`, or `build` refuses with
`EmbedError::MissingExecutionBudgets`. A direct client takes it too:
`DirectLlmClient::new(provider, model, budgets)`. A host with no numbers of its
own passes the named preset, `ExecutionBudgets::recommended()`; no measurement
backs the preset's values. `ExecutionBudgets::new(config)` validates an
`ExecutionBudgetsConfig`, whose every field is stated:

| Field | `recommended()` | Bounds |
| --- | --- | --- |
| `model_total` | 10 min | One model call, over throttle, backoff and every provider attempt. |
| `tool_check_phase` | 10 s | One tool call's argument or result transforms and checks, together, within its member execution deadline. |
| `turn_hook_phase` | 60 s | One before-turn, checkpoint or after-turn phase, all callbacks together. |
| `stop_grace` | 2 s | Spent once after a stretch ends at its limit or on cancel, to collect evidence. |
| `provider` | see below | `ProviderAttemptLimits` |
| `agent_frame_switch_limit` | 16 | A chain of agent frame switches; the follow-on at this depth stops with `AgentFrameSwitchLimit` before calling the model. |

`ProviderAttemptLimits::new(per_request, response_start, chunk_idle,
max_attempts)` states the attempt limits; `ProviderAttemptLimits::recommended()`
is `RequestBound::Bounded(Duration::from_secs(300))`, 2 min to the response
start, 2 min of chunk silence and 4 attempts. Select `RequestBound::Unbounded`
to allow attempts with no whole-request timer. Response-start and chunk-idle
bounds are still required and finite. Finite bounds are clipped to the call's
remaining `model_total`; that separately recorded hard deadline always applies.

A route's `ProviderReliability::request_timeout` is a `RequestTimeout` policy:
`Inherit` uses the runtime request bound, `Bounded(NonZeroU64)` states positive
milliseconds, and `Unbounded` explicitly disables that timer. Omission defaults
to `Inherit`. A bounded runtime rejects an unbounded route, just as it rejects
a route bound above its ceiling. For an interactive stream without a request
timer, choose `RequestBound::Unbounded` on the runtime and either `Inherit` or
`Unbounded` on its route. The `request_timeout(policy)` builder sets this field.

The serialized field is now `{"policy":"inherit"}`,
`{"policy":"bounded","milliseconds":60000}`, or `{"policy":"unbounded"}`.
Numeric, null and zero request-timeout values are unsupported. Finite runtime
and managed route request bounds have the existing 30-day safety maximum.
`Unbounded` has no request-specific maximum; the finite `model_total` still
limits the call. Response-start and idle failures keep their timeout causes
and retry classification for every HTTP/SSE and Codex WebSocket adapter.

`new` refuses with `ExecutionBudgetsError`:

- `OutOfRange`: a bound below 1 ms or above `MAX_EXECUTION_BUDGET` (30 days);
- `DefaultExceedsCeiling`: a bounded `provider.per_request` above
  `model_total`, or a provider sub-bound above a bounded `per_request`;
- `SumOverflows`: a stretch plus its grace exceeds the maximum;
- `UnboundedRetry`: `max_attempts` is 0 or above `MAX_PROVIDER_ATTEMPTS` (16).

Limits are recorded before their work starts and never refreshed. A nested
stretch takes `min(own, enclosing remaining)`. An expired limit found on load
settles at once.

### Tool bounds are host declarations

Every tool definition is made by `with_execution(Duration)`, which supplies
its required body bound: `ToolDefinition::raw` answers a `ToolDraft`, and
only a draft with its bound is a definition. The manifest records
`execution`. A tool that can defer declares it with
`with_declaration(declaration, Some(ParkBound))`, either `Within(Duration)`
or `UntilScopeEnd`; a non-deferring tool passes `None`. A missing park bound
is refused as `MissingBound { tool, bound }`, and a park on a non-deferring
tool as `ParkWithoutDeferral { tool }`, when the definition is built; a
stored manifest without its bounds does not decode. An isolated tool names
its process engine with `isolated_in(engine)` and registers only where that
engine is registered (`UnregisteredIsolationEngine { tool, engine }`). No
manifest reaches admission without them, so no call is refused for them.
Lash defaults neither bound and caps neither with a tool or wait ceiling.
The shipped tools follow the same rule: an MCP tool's body bound is its
server's `call_max_total_timeout_ms`, and `processes.await` parks
`UntilScopeEnd`. A process engine sets each step body's bound through the
required `EngineSteps::execution(kind)`. A turn's parks are scoped to the
turn: an `UntilScopeEnd` park is revoked when the turn that admitted it ends.

The body bound ends the running body. The park bound is independent, so a
human approval can outlive a short tool body. Admission computes a bounded
park's deadline once and records it; recovery and takeover do not refresh it.
`UntilScopeEnd` has no deadline and is revoked when the owning scope ends.
An expired park settles `TimedOut` with a wait cause, distinct from a body
timeout. Hosts set engine wait bounds explicitly too.

Long work can use a declared process start, an isolated tool on a host engine,
or a Pending-capable tool whose external work settles through its completion
key. Nothing is promoted at runtime: the declaration decides.

## 7. Read tools

Host data reads are package tools declared with `ExecutionPolicy::Read` and
registered with `ToolProviderRegistration::with_reads`. The machine answers a
planned read inside its statement, without a park or journal record (ADR 0144).
Readers must be pure and tolerate repeated reads after recovery. A host needing
stable answers carries an immutable revision in its input or run context.

RLM supplies no transcript read (ADR 0147). A cell sees its current frame's
turns and the session's variables; a value a later cell needs lives in a
variable, and `control.continue_as` carries chosen values into a fresh frame.

## 8. Operations

### DurableConfig

`DurableSettings::standard()` is the production preset and the builder's
omission default. `DurableSettings::development()` reduces claims to 2,
active actors to 8, group members to 8 and cascade batches to 32, keeping
all other standard values. Both are plain data, and every field remains
configurable. FIG-5167 measured wake behavior under the standard leases;
the other numerical capacities and timing choices lack workload measurements.

For SQLite, `SqliteConnectionPolicy::standard(synchronous)` selects the
production connection preset, and `::development(synchronous)` selects one
reader and a 2 s busy budget. The host states filesystem synchronization.
`StoreOptions::development(synchronous)` uses uncompressed blobs;
`SqliteStoreSetOptions::development(synchronous)` also keeps the standard
migration backup (two complete backups beside the store). Backup location
and count remain configurable.

The connection policy's `operational` field exposes readonly busy/cache,
statement cache, checkpoint pacing/busy, WAL retry, wake poll/retention/reopen,
lock attempts and polling, migration polling, checkpoint/graph chunks,
event-release paging and write batches. The writer commits the writes already
queued when it becomes free as one transaction, each in its own savepoint
(`write_batch_members`, 32; `write_batch_rows`, 4,096): one sync under `Full`
covers them all, no write answers before that commit returns, and nothing waits
for a batch to fill. `SqliteOperationalSettings::standard()` documents every
value and its evidence; `::development()` reduces working capacities.
FIG-3975 found a 16-statement cache evicted the store's statement mix, leading
to 256 in standard. The timing and reduced development values are unmeasured.
Handles on one file share a checkpoint worker and resolve its timing and
threshold to the smallest values requested during that worker's lifetime, rather than a
first-opener policy.
Reopened handles retain their selected options.

`DurableSettings` is plain data. The builder validates it into a
`lash::durable::DurableConfig`, and `DurableConfigError` names the first broken
rule.

| Field | Default | Meaning |
| --- | --- | --- |
| `lease` | see [§1](#heartbeat-self-stop-and-reap) | Node-lease timings (`LeaseSettings`). |
| `claim_batch` | 16 | The most actors one claim takes. Must not exceed `max_active`. |
| `max_active` | 256 | The most actors a node runs at once. |
| `owner_retention` | 1 hour idle, 10,000 residents | Overridable `OwnerRetentionSettings`: idle time and nonzero resident session/process count. |
| `max_ingress_bytes` | 64 MiB | Overridable nonzero serialized host-item limit, at most the runtime commit byte budget. |
| `activation_loop_budget` | 8 | Claims in a row without progress before an actor parks with `ActivationLoop`. |
| `group_commit` | 64 members, 5 ms | Finished round members commit in batches (`GroupCommit { max_members, window }`). The window must be shorter than `lease.claim_poll`. |
| `snapshot_every_fuel` | 1 000 000 | VM fuel spent without an effect after which a quiet point snapshots anyway. |
| `cascade_batch` | 256 | How many `Until` children one cascade transaction marks. |
| `notifier` | `AfterCommit` | `AfterCommit` publishes wake hints and holds the liveness lock, on PostgreSQL and on a SQLite file; `PollOnly` relies on the claim poll alone. |
| `retirement.every` | 1 s | How long the node's retirement task waits between sweeps once it caught up. |
| `retirement.batch_nodes` | 256 (at most 4 096) | The most page nodes one retirement step, one transaction, deletes. |
| `retirement.steps_per_sweep` | 16 (at most 256) | The most steps one sweep runs. |
| `retirement.backlog_limit` | 10 000 holds and frontier nodes (at most 1 000 000) | Above it the node claims no new actors and sweeps without waiting, until the backlog falls back under. |

Counts must be at least 1 (`Zero`), positive durations at least 1 ms
(`BelowResolution`), retirement values at most their hard maxima
(`AboveMaximum`), plus `ClaimBeyondCapacity`, `GroupWindowNotBeforePoll`
and the lease rules (`Lease(LeaseConfigError)`).

### Owner retention

`DurableSettings::standard()`, `default()` and `development()` supply generous
starting points: `owner_retention.idle` is one hour,
`owner_retention.max_resident_owners` is 10,000, and `max_ingress_bytes` is
64 MiB. `OwnerRetentionSettings::standard()` and `Default` supply the same
retention policy. Hosts override these plain fields directly, for example
`settings.owner_retention.idle = Duration::from_secs(300)`.

`CommitBudget::standard()` and `Default`, also used by `LashCoreBuilder`,
default to 64 MiB and 512 recorded rows. These are generous starting points;
tune them for the deployment. A host choosing a smaller commit byte budget
must also lower `max_ingress_bytes` to fit it. The aggregate commit budget
still applies to the complete commit.

The resident count includes session and process owners. A claim past the
count requests the LRU idle owner's release and starts its replacement only
after that activation drops every reference. With all owners busy, durable
mail stays queued until a slot frees. A synchronous admission may return
retryable `OwnerCapacityExceeded { actor, resident, capacity }`. No busy
owner waits for admission. Zero idle time releases immediately; positive
idle durations must fit signed 64-bit milliseconds.

One serialized host message or queued command larger than `max_ingress_bytes`
is refused before enqueue with typed `HostPayloadTooLarge`. Kernel function
values, imperative state commands, snapshot crossings and VM heaps retain
their existing limits. There is no per-allocation host byte ledger.

Owner probes attribute reads by actor/epoch: `OwnHeadReload`,
`OwnCommittedPhaseReload`, `ExternalFrontierReads`, `RecoveryLoads`,
`HistoricalPinReads`, process record/driver/root reloads,
`CompletedRunRowsReplayed`, `ContextBytes`, `ActiveRunRows`, and `VmPages`.
Hydration, requested context and active facts are separate from warm own-state
reloads. The crash harness records acknowledged/adopted cut occurrences and
fails when a selected hook never fires.

### Page retirement

A replaced or released heap root is not deleted in the transaction that
replaces it: it becomes a retirement hold
([ADR 0145](../adr/0145-kernel-execution-and-its-checkpoints-share-one-persistent-heap.md)
§8). A root a worker is reading is protected by a root lease, a catalog row
the activation's parent renews at a third of its 60 s time to live; an
expired lease reads nothing and fences the activation, and retirement reaps
it. Each node runs a
retirement task beside its lease renewal. A sweep runs up to
`steps_per_sweep` bounded steps, each one transaction that deletes at most
`batch_nodes` unreachable pages and reaps expired root leases, and stops
early once a step finds nothing to do. What a step does not reach waits on
its own frontier for the next one. On PostgreSQL steps run one at a time
under an advisory key; after it a step waits on nothing: it passes over
rows another transaction holds (`SKIP LOCKED`) and writes only its own
frontier. Publications and lease releases lock every node they hold before
they write a hold row. So a step never deadlocks with a publication and
never stops one; at most a publication waits for a step's transaction to
end. When the store's
`retirement_backlog` is above `backlog_limit`, the node takes no new actors
(its running actors carry on) and sweeps back to back until the backlog is
under the limit again. A node boots held back and claims only once its first
backlog read answers under the limit. A failed step is retried at the next sweep.

Page staging is bounded per activation by the worker service's
`staging_quota_bytes` (1 GiB); passing it is a host outcome, the
activation's `StagingExhausted`.

### Parked work and redrive

An actor whose claims commit nothing parks at the activation-loop budget. A
claim that makes progress resets the count, so node restarts during a deploy
do not park long work. A process also parks when no engine of its kind is
installed on the claiming node (`UnknownEngine`), when its state is in a format
the engine does not read (`UndecodableState`), or when its engine refused a
transition (`AdvanceRefused`). Each park is recorded in the park feed with its
`ProcessParkReason`, and a read of the process
(`Processes::get`, `list`, the work snapshots) carries the same reason in
`ObservedProcess::park` while the park stands. The lifecycle status does not
change: a parked process still reads as the `running` or `waiting` its record
says.

A parked process holds what it holds and runs no engine code until an operator
acts. `LashCore::parked_work()` lists parked turns.

A parked session waits for an operator too, except when another build may
clear what parked it (FIG-6310). Every node's boot redrives, before its first
claim, each session parked `PassLoop` or `UndecodableState`
(`SessionParkReason::REDRIVEN_AT_BOOT`). Every failure of such a park ended
inside a live claim, so the node that met it survived it. The next claim runs
the unfinished turn again from its committed state. A deploy that fixes the
failure therefore resumes the turn without an operator, and a session that
still fails parks again at its budget, at most once per boot. Sessions parked
`ActivationLoop` or `CancelLoop` are not redriven at boot. Their claims
committed nothing, which is also what a node's death leaves, so redriving them
at every boot could crash each new node in turn. For those, and for any
session a boot's redrive did not clear, the fallback is the operator path:
`DurableSession::park_reason()` names the park, and `DurableSession::redrive(requester)`
clears it on a build that carries the fix.
`LashCore::processes().redrive(process_id, requester)` (`lash::process::Processes`)
clears a process's park and activation count so its engine runs again from
committed state; it answers whether the process was parked. Cancelling a
parked process ends it without running its engine.

A park never strands a stop (FIG-6099). A process's cancel, a session's
`Immediate` turn cancel or its close (the close request is a control wake)
that reaches the actor after the owner read it for its park leaves the actor
ready, its park kept, and its next claim runs the stop. A session's park reads
the stop first: it runs a stop it finds rather than park over it. A stop that
fails the activation-loop budget's claims in a row (a claim of a node that only
ends it, or the session's engine-free stop after its own budget) parks
`CancelLoop` and stays recorded: the cancel on the process record or the turn,
the close on the session's mail or close row. A redrive runs it again; so does
a further close request of a session. A repeated cancel request is answered
from the recorded one and wakes nothing.

### Cancel and the cascade

A cancel request is a mailbox row; it wakes its actor, including a parked one.
A turn, a session close and a process terminal end their `Until` children
through a batched cascade: each `cascade.batch` transaction cancels at most
`cascade_batch` children and keeps a cursor, and the ending actor stays
runnable until the cursor is drained.

### Format compatibility

Every actor records the format set its state is written in. A node registers
the format sets its build decodes (`Backend::formats`) and claims only actors
in them. An actor in a format no running node decodes stays unclaimed until a
build that decodes it runs. Changing kernel code never needs a drain. Changing
a durable format does, under [ADR 0106](../adr/0106-durable-formats-upgrade-by-migration-or-drain.md):
the old build [drains by release](#drain-by-release) and finishes nothing by
hand.

### Two non-guarantees

**A terminal does not mean a quiescent subtree.** A process's terminal, a
turn's commit or a session's close begins the cascade over its `Until`
children; it does not wait for them to stop. A child may still be running,
cancelling within its grace, or waiting in the cascade's queue when the
parent's terminal is visible. A host that needs "everything under this scope
has stopped" reads it: the durable store's `live_until_descendants(scope,
limit)` (`lash::durable::DurableReads`, through `Backend::durable()`) lists up
to `limit` non-terminal processes whose lifetime is `Until` the scope or one
of its descendants. Empty means the subtree has ended. There is no durable
"scope settled" fact in 1.0.

**Host event ordering is host policy.** The host records callbacks and due ticks
in its own ledger and decides their order before keyed delivery. Lash orders
accepted session input through its ingress; it does not order product events
from independent producers.

## 9. Events, routing and scheduling

[ADR 0137](../adr/0137-the-host-owns-events-routing-and-scheduling.md) defines
the host-events contract. Approvals and callbacks use deferring tools with
completion keys, a stable `call_id` and caller context. Process-end notices use
committed lifecycle cursor reads followed by `send().id(TurnId)`. Hosts own
trigger registrations, source provisioning, input mappings, timers and enabled
state, and deliver process starts with `with_host_start_key` after committing
the occurrence or tick. A start-args check validates partial mappings at
registration and complete arguments at delivery against the definition's
signature. The host chooses the started process's tools explicitly and keeps
its definition pinned while its registration needs it.

Delivery from the host comes after the commit, deduplicated by keys. Persist
unfinished deliveries and reconcile them after a restart. Record a start's
returned process binding before permitting pruning: a start key deduplicates
only while its process is retained, and Lash has no host delivery receipt
table. A later duplicate consults the host's saved binding.

`ProcessExternalRef` carries only `backend`, `id` and `segment_ordinal`.
Hosts that used `ProcessExternalRef.metadata` keep that metadata in their own
adapter keyed by process id.

The process lifecycle log has a closed vocabulary: started, waiting with
`call_id` and `tool_id`, resumed, wait cancelled, effect outcome, effect
omissions, cancel requested, observer added and removed, external reference set, and completed,
failed, cancelled or abandoned. Waiting facts carry no completion key. Hosts
reconcile retained process and turn cursors, acknowledging a page only after
recording it or completing its idempotent actions. Best-effort observation is
freshness; it does not replace reconciliation.

The [workbench approval tool](../../examples/agent-workbench/src/approvals.rs)
and [host routes and timers](../../examples/agent-workbench/src/main_sections/)
show these patterns. Internal actor notifications and `NodeWakes` remain
runtime transport; they do not route product notices.

## 10. Provider credentials

Lash runs no OAuth. The host owns login, refresh, rotation and secret
storage, and gives each provider a `lash::provider::TokenSource`. Lash keeps
no token cache and records no token.

**The seam.** `TokenSource::token(TokenRequest)` returns a `ProviderToken`: a
secret, an optional expiry, an optional bound account (Codex sends it as
`ChatGPT-Account-ID`) and an optional non-secret principal that partitions
provider-side caches such as Google uploads. `ProviderToken` has no
`Serialize`, and its `Debug` prints neither the secret nor the account.

**When lash asks.** Before every model-call attempt, with
`TokenRequestReason::Current`. Answer from the host's own cache; it must be
cheap and safe to call concurrently. When the answer expires within the
provider's selected `TokenPolicy::expiry_skew`, lash asks once more with
`Expiring`. `TokenPolicy::standard()` selects 30 s; the provider constructor's
`with_token_policy` selects a different skew. When the provider answers 401
before any output, lash asks once with `Rejected` and `stale` set to the token it sent,
then resends the admitted body once. A 401 after output started is surfaced,
never retried, and 403 is never retried.

**Compare and refresh.** If `stale` is no longer the host's current token,
return the current one without refreshing. Returning the same secret means
"nothing newer", and lash surfaces the provider's 401. Concurrent 401s on one
provider make one host call between them: lash single-flights `Rejected` per
provider and numbers the tokens it sees, so per-token state (the Codex
WebSocket session cache) evicts what an older token opened.

**Persist before you hand out.** A rotating refresh token is dead once the
identity provider answered. Write the rotation to the host's store before
returning the new access token.

**Failures.** The host classifies its own failure as a `TokenError`:

| `TokenErrorKind` | Failure code | Retry |
|---|---|---|
| `ReauthRequired` | `lash:credential_reauth_required` | never |
| `Transient { retry_after }` | `lash:credential_source_transient` | retried, after `retry_after` when given |
| `Unavailable` | `lash:credential_unavailable` | never |

Lash keeps no sticky failure: the next attempt asks again, so a re-login takes
effect on the next call.

**Static keys.** A fixed API key is a `ProviderToken`, which answers every
request with itself. `AnthropicProvider::new(key)`, `OpenAiProvider::new(key)`
and `OpenAiCompatibleProvider::new(key, base_url)` wrap one;
`with_token_source` takes a host source instead. `CodexProvider::new` and
`GoogleOAuthProvider::new` take only a source.

**Rebuilding a provider.** `Provider::serialize_config` returns no credential.
A host that persists provider configuration re-attaches its token source when
it rebuilds the provider.

[`examples/codex-host-auth`](../../examples/codex-host-auth) is a complete host
source for Codex: the ChatGPT device-code login, a refresh on `Expiring` or
`Rejected`, and a rotation written to the host's file before use.

For Anthropic subscription tokens, choose the placement explicitly with
`AnthropicProvider::with_token_source(source).with_auth_scheme(AnthropicAuthScheme::Bearer)`.
The default `ApiKey` scheme sends `x-api-key`; `Bearer` sends `Authorization: Bearer`
and adds the `oauth-2025-04-20` Anthropic beta token. The non-default scheme is
included in `serialize_config`, without the credential.
Both schemes ask the same host-owned token source before each attempt and on rejection.
Credential header values carry sensitivity through transport and recording, including
custom OpenAI-compatible auth headers and Codex's bound account ID.

## 11. Local values and host transport

[ADR 0136](../adr/0136-hosts-own-their-wire-contracts.md) owns the transport
boundary. Lash returns local typed Rust values; the host defines its DTOs,
authentication, transport and client compatibility policy. Serialization on a
core type does not promise a stable engine wire contract. For an example,
[the workbench observation envelope](../../examples/agent-workbench/src/main_sections/routes/observation_envelope.rs)
projects the local feed into host-owned NDJSON. The VM worker IPC contract is a
separate execution boundary, described in
[ADR 0123](../adr/0123-model-code-runs-in-resettable-worker-processes.md).

### Typed history paging

`DurableSession::history(anchor, budget)` is a typed store read that opens no
runtime. Start with `lash::persistence::HistoryAnchor::Head` or `Node`, and
supply both nonzero `HistoryBudget::max_nodes` and `max_bytes`; there is no
unbounded history request. The `HistoryPage` carries decoded records in
descending generation, its pinned leaf, a stop reason and `next`. Continue with
`HistoryAnchor::Cursor(next)` while `next` is present.

A paging run follows one ancestry across frames and fork boundaries. Its
cursor is session-bound and pins the leaf and lineage; a later append does not
move that run's starting point. Fork ceilings constrain inherited ancestry,
and each `HistoryNode::owner_session_id` identifies the stored row's owner.
Product projection and cursor delivery to clients belong to the host.

`committed_turns_with_calls(after, limit)` pages the same committed turns
with each returned turn's tool-call bodies decoded. Consume call bodies with
the committed-turn page, or keep each call's turn address (`TurnToolCallRef`)
for later `read_tool_call`/`read_tool_calls` reads; a bare call id cannot be
resolved without scanning.

### Reading a call's settle verdict

`DurableSession::read_tool_call(&TurnToolCallRef { turn_id, call_id })`
returns Lash's committed verdict in `ToolCallRead::Settled { record }`.
Inspect `record.output.outcome`: for `lash::tools::ToolCallOutcome::Failure`,
the `ToolFailure` carries `code`, `message` and a typed `cause`. This is the
settled result even when the host answered Success: a value its output schema
rejects is recorded as a failure with code `tool_outcome_not_declared`, a
message naming the tool and mismatch, and
`ToolFailureCause::Declaration { refusal: DeclarationRefusal::OutputValueMismatch { .. } }`.
The host's original answer does not establish the committed outcome.

Each committed cell's `ExecutedCall.outcome` is an
`ExecutedCallOutcome::Ok {}` or `Failed { code, message }`, derived from that same
settled output. A mirrored cell therefore has the refusal reason without
another read per card. The message keeps the longest UTF-8 prefix within
`ExecutedCallOutcome::MAX_MESSAGE_BYTES` (4,096 bytes); the ledger retains the
whole message and typed cause. A cancelled call is failed with code
`cancelled` and its recorded cancellation message.

To read those full details, take `call_id` from `ExecutedCall` and `turn_id` from the
enclosing transcript entry's provenance. `read_cell_tool_calls(turn_id,
&cell.calls)` reads all of that cell's host call records in order. These
reads use the turn's retained call transcript (an open turn's ledger) and
work through a cold durable handle;
the failure details have one source in `ToolCallRecord.output`.

A host projector can consume ordinary committed pages without decoding call
bodies. Retain each call's complete address alongside its transcript placement,
scoped to the session. The projection rows, addresses and next cursor must commit
in one host database transaction: a crash must not advance the cursor past an
address that was never saved. In this example, `projection` and its transaction
methods belong to the host; the page, transcript and address types are Lash APIs.

```rust,ignore
use lash::transcript::{TranscriptBlock, TranscriptItem};
use lash::TurnToolCallRef;
use std::num::NonZeroU32;

let after = projection.cursor(session.session_id()).await?;
let page = session.committed_turns(after.as_ref(), NonZeroU32::MIN).await?;
let mut tx = projection.begin(session.session_id()).await?;
for turn in &page.turns {
    tx.apply_turn(turn).await?;
    for entry in &turn.entries {
        let call_ids: Vec<_> = match &entry.item {
            TranscriptItem::Message(message) => message.blocks.iter().filter_map(|block| {
                match block {
                    TranscriptBlock::ToolCall { call_id, .. } => Some(call_id.clone()),
                    _ => None,
                }
            }).collect(),
            TranscriptItem::Cell(cell) => cell.calls.iter()
                .filter_map(|call| call.call_id.clone()).collect(),
            TranscriptItem::Suppressed(_) => Vec::new(),
        };
        for call_id in call_ids {
            let address = TurnToolCallRef {
                turn_id: turn.turn_id.clone(),
                call_id,
            };
            tx.save_call_address(&entry.entry_id, &address).await?;
        }
    }
}
tx.save_cursor(&page.next).await?;
tx.commit().await?;

// A later reader loads the saved pairs and passes them unchanged.
let addresses: Vec<TurnToolCallRef> = projection.call_addresses(session.session_id()).await?;
let reads = session.read_tool_calls(&addresses).await?;
for (address, read) in addresses.iter().zip(reads) {
    match read {
        lash::ToolCallRead::Settled { record } => render_tool_card(address, &record),
        lash::ToolCallRead::NotFound { .. } => render_missing_call(address),
        lash::ToolCallRead::NotYetSettled { .. } => render_pending_call(address),
    }
}
```

Here `session` is the same `DurableSession` for both reads. A saved address
does not pin retention; handle `NotFound` when its ledger has been removed.
For an immediate renderer that needs the bodies, opt into the enriched page
and render each turn with its decoded calls instead:

```rust,ignore
let page = session
    .committed_turns_with_calls(after.as_ref(), NonZeroU32::MIN)
    .await?;
for committed in &page.turns {
    render_turn(&committed.turn, &committed.tool_calls);
}
// Persist page.next with the host's consumed-page state after applying all turns.
```

[Facade history rustdoc](../../crates/lash/src/durable_session.rs),
[typed pages and cursors](../../crates/lash-core-store/src/store/history.rs) and
[ADR 0112 §6](../adr/0112-the-store-is-multi-session-and-a-session-is-resident-from-its-current-frame.md#6-load_ancestors)
define this contract.

## 12. Host prompt policy

The host supplies `lash::prompt::PromptPlan` at creation with
`SessionCreation::with_prompt_plan`. Core records the plan separately from
plugin namespaces. Installed plugins register keyed sections and wrappers;
the plan selects their ordering and placement for each call's purpose.
Change the recorded plan through `lash::config::SetPromptPlan` in a
`ConfigTransaction`, applied by the session command lane. A run keeps its
admitted config snapshot; the transaction changes subsequent admissions.

Submit a config transaction without waiting inside a tool in that same
session:

```rust,ignore
let config = session.admin().config();
let expected_revision = config.revision().await?;
let submission = config.submit(
    lash::config::ConfigWrite::new(command_id, expected_revision),
    lash::config::ConfigTransaction::of(lash::config::SetPromptPlan { plan }),
).await?;
// Return this durable status/receipt to the host; the tool must now finish.
// The host may later settle a Pending receipt from outside this run.
Ok(submission)
```

A transaction queued behind an active run cannot settle until that run
releases the session head, including when it is parked. Awaiting
`submission.await_outcome(&config)` in a same-session tool creates a cycle:
the run waits for the tool, while settlement waits for the run. Durable
submission is the point at which that tool returns; an independent host
caller can retain the receipt and choose when to await settlement.

Acceptance predicts neither application nor the next revision. An accepted
command can settle `Stale` (another transaction changed the revision) or
`Refused` (an owner rejected it). Use the `revision` from an `Applied`
outcome; do not send work against a predicted `expected_revision + 1`.
Before preparing a fresh transaction after a stale or refused outcome, read
`config.revision()` again. A delivery retry reuses the original command id,
expected revision and content; a revised transaction needs a fresh id.

Composition occurs for each new model-call admission. Core records the
resolved plan, section text, wrapper outputs, request template and response
contract with that call. A resend loads that record without rerunning a
renderer or wrapper; the next new call composes again. Initial-instruction
placement sets the request's instruction field. Current-context placement
extends an existing context prefix or adds a trailing user message, following
the protocol's placement implementation.

`SessionPromptAdmin::plan`, `catalog` and `preview` inspect policy and resolution;
`preview` renders nothing and admits no call. `snapshot(run, call)` reads a
retained call's exact text. See [facade prompt rustdoc](../../crates/lash/src/lib.rs),
[creation](../../crates/lash/src/session.rs),
[core plan command](../../crates/lash-core-execution/src/plugin/config/core.rs),
[prompt administration](../../crates/lash/src/admin/prompt.rs),
[composition and recording](../../crates/lash-core-execution/src/plugin/prompt/composer.rs)
and [placement](../../crates/lash-sansio/src/sansio/turn_protocol.rs).
[ADR 0133](../adr/0133-prompt-sections-are-keyed-trusted-and-placed-by-the-host.md)
defines keyed section and wrapper ownership.

## 13. Attachment delivery and provider sends

### Refs become live values per attempt

Session input holds `AttachmentRef` values: content identity, media type, byte
length and optional typed metadata. A provider lowers the composed request to
`RecordedRequestTemplate`; each `AttachmentSlot` records a ref, acceptance and
a pinned codec. The call's admission retains those refs and literals, without
recording a delivered URL, provider file id or inline body.

For each attempt, the host store supplies `Delivery` values through
`SlotDeliveries`. It checks acceptance and the requested
`DeliveryContext::valid_through_ms` horizon. Missing content, a refused form or
an insufficient horizon produces a typed delivery failure before send.
The provider checks the delivered form against the slot's acceptance and
encodes it through the recorded codec into a transient `LiveRequestBody`.
Deliveries and the filled body live only for the attempt; a resend obtains
fresh delivery values for the recorded slots.

[ADR 0135](../adr/0135-attachments-are-durable-refs-delivered-by-the-host-store.md),
[host-store delivery](../../crates/lash-core-store/src/attachments/delivery.rs),
[slot fill](../../crates/lash-core-llm/src/provider/slot_delivery.rs) and
[facade provider types](../../crates/lash/src/lib.rs) define these seams.

### Implementing the provider boundary

`Provider::send(&mut self, body: &LiveRequestBody, context: ResponseContext)`
takes the body admitted for the call and the facts needed to interpret its
response. `ResponseContext::scope` identifies the call and attempt;
`ResponseContract` pins the model, requested output and offered tools' input
schemas. Recorded context has no senders. Each live attempt adds its stream
and trace senders through `ResponseContext::with_senders`.

The body is the call's statement of what it asks. An implementer that needs
request content decodes that body; an in-process provider using canonical
lowering may use `LiveRequestBody::canonical_request`. Reconstructing another
request beside it would let a resend disagree with the admitted call.
See [provider trait](../../crates/lash-core-llm/src/provider/traits.rs),
[response context](../../crates/lash-protocol-types/src/llm/response_context.rs) and
`lash::provider` in [facade rustdoc](../../crates/lash/src/lib.rs).

### Canonical forwarding retains the attempt

Use `canonical_request(body, context)` or `canonical_call(body, context, ...)`
when decoding a canonical send. Both retain `context.attempt_control` in
`LlmRequest::attempt_control`, alongside its live stream and trace senders.
The default `Provider::complete` carries that same control into the leaf's
`ResponseContext`. A decorator forwarding a decoded request must preserve
this field: header `capture_metadata` and cooperative cancellation then reach
all participants in the same attempt. Explicit request literals initialize
`attempt_control: Default::default()` for a new call. The control is skipped
by serialization; no stored request or wire format changes.

A returned `Ok(LlmResponse)` is the fact that answers the logical call and
completes its attempt (`Answered`, `Completed` / `TerminalObserved`). This
includes `LlmResponse::default()`: no stop reason or usage does not establish
an interrupted send. It reports no usage, not an observed zero-token bill.
`TerminalObserved` here means the provider future returned its response;
the response's dialect stop reason remains unchanged. A transport that
cannot accept an incomplete stream returns an error with its partial response
under the selected stream-termination policy below.

### Completion ownership and dropped direct calls

Lash drives admitted turn model calls submitted through `send()` and direct
model effects made inside a Lash tool, hook or process execution, including
`ToolDirectCompletionClient::complete` and execution-bound
`DirectCompletionClient::direct_completion`. Those calls retain an independent
provider task: cancellation or dropping the execution's awaiter signals that
task, which keeps polling through the bounded drain and delivers the result
or `Provider::attempt_ended` terminal described in
[ADR 0127](../adr/0127-usage-is-result-data-hosts-meter-spend.md#the-settle-point).
Dropping a send follower does not cancel its admitted turn.

A host polling `ProviderHandle::complete`, `Provider::complete`,
`Provider::send`, or a standalone `DirectLlmClient::complete` owns that future.
Any bounds provided by that API or transport apply while the future is
polled; Lash retains no independent task if the host drops it. No automatic
abandoned-terminal callback follows that drop. Prefer a host-owned task that retains and awaits
the completion even if its UI or request awaiter disappears. If the host
must drop a direct completion, its owner must settle the reserved attempt
itself in its provider decorator or receipt store, using the call/attempt
identity and partial receipts it retained before the drop. A raw provider
owner can deliver `attempt_ended` after dropping the borrowed future;
`ProviderHandle` exposes no such post-drop delivery operation. Make this
settlement idempotent and reconcile any receipt whose result was lost. A
process crash also requires host reconciliation; the callback is not durable.

### Failed-turn messages are typed classification text

Since FIG-6565, the settled `TurnOutcome`'s failure message is
`TurnErrorCause::detail()`: Lash's text for its typed cause, such as
"provider authentication rejected". It is not `LlmTransportError::message`
or the provider's response body. Child-turn failure projection uses the
same committed code and detail. This avoids committing provider bodies,
credentials or user content into the terminal.

A host that wants to display the provider's own text can capture the returned
`LlmTransportError::message` (and any permitted raw evidence) in its provider
`send` decorator before Lash projects the error. The host decides what is
safe to display and retains that text itself if it must survive a reload.
The committed outcome and normalized call-attempt error contain typed facts,
not that original text, so it cannot be recovered from those reads later.

## 14. Stream termination policy

An omitted route/model policy selects
`lash::provider::StreamTermination::EofTolerated`: a clean EOF can complete retained
output without a dialect terminal event. EOF tolerance does not make an empty
or malformed response valid. A host requiring terminal evidence explicitly
selects `RequireTerminalEvidence`; the model's
`LlmProfileCapability::stream_termination` overrides the route choice.
OpenAI-compatible routes configure it through `OpenAiCompat`, and Anthropic
and Google expose it on their provider configuration. The OpenRouter preset
explicitly remains strict.

Under strict policy, missing dialect evidence is a typed stream failure with
partial output retained for accounting, not a completed protocol response.
Cancellation and abort-usage drain retain their own rules. See
[ADR 0036](../adr/0036-stream-termination-is-explicit-dialect-policy.md),
[policy](../../crates/lash-sansio/src/llm/capability.rs) and
[OpenAI route resolution](../../crates/lash-provider-openai/src/config.rs).

## 15. Hosting cutover map

Use this index when replacing a pre-cutover integration. Each row points to
the current boundary and its owning contract.

| Contract | Hosting entrypoint | Authority |
| --- | --- | --- |
| Host events, routing and scheduling | [§9](#9-events-routing-and-scheduling) | [ADR 0137](../adr/0137-the-host-owns-events-routing-and-scheduling.md). |
| Host DTOs and transport | [§11](#11-local-values-and-host-transport) | [ADR 0136](../adr/0136-hosts-own-their-wire-contracts.md). |
| Typed bounded history | [History paging](#typed-history-paging) | [Facade history](../../crates/lash/src/durable_session.rs). |
| Attachment refs and store delivery | [§13](#13-attachment-delivery-and-provider-sends) | [ADR 0135](../adr/0135-attachments-are-durable-refs-delivered-by-the-host-store.md). |
| Host token source | [§10](#10-provider-credentials) | [Credential seam](../../crates/lash-core-llm/src/provider/credential.rs). |
| Host prompt plan | [§12](#12-host-prompt-policy) | [Core config command](../../crates/lash-core-execution/src/plugin/config/core.rs). |
| Selected EOF policy | [§14](#14-stream-termination-policy) | [ADR 0036](../adr/0036-stream-termination-is-explicit-dialect-policy.md). |
| Shared execution budgets and declared body/park bounds | [§6](#6-execution-budgets) | [Execution budgets](../../crates/lash-sansio/src/execution_budgets.rs). |
| Required choices and explicit presets | [Required choices](#required-choices-before-serving) | [Core validation](../../crates/lash/src/core/runtime_host_config.rs). |
| Admitted body and response context | [Provider sends](#implementing-the-provider-boundary) | [Provider trait](../../crates/lash-core-llm/src/provider/traits.rs). |


## 12. Data admission, delivery and presentation budgets

A successful `LashSession::put_attachment` returns `AttachmentPutReceipt`.
Its `reference` is the stored attachment; `byte_delivery` is a typed advisory
about one occurrence's byte delivery under the configured read policy.
`ExceedsBudget` names `byte_len` and `max_bytes`; `WithinBudget` names the same
quantities. `max_bytes` is the effective blob limit: the smaller of the blob
cap and what the request budget can deliver after reserving this ref's
MIME/label envelope. URL stores report `NotApplicable`. Put admission and
byte delivery remain independent (ADR 0058): an over-budget blob is still
stored successfully. The advisory does not promise that multiple attachments
or repeated occurrences fit together, or that a provider accepts the MIME.

The shared `AttachmentReadPolicy::byte_delivery_cost(capacity, byte_len,
occurrences)` charges `capacity + occurrences * 12 * ceil(byte_len / 3)`.
Retained capacity is charged once per delivery group; three base64-sized
values cover encoding, assembled requests and transport framing/diagnostics.
Base64 characters need no JSON escaping. Each occurrence also reserves
`1024 + 24 * (MIME bytes + label bytes)` through `delivery_envelope_cost`.
Overflow refuses delivery. These are payload-work allowances, not whole-process
RSS or a bound on the full prompt.

For example, at 128 MiB/request, one tightly allocated `image/png` blob with
no label has a largest byte-delivered size of **26,843,296 bytes** (about
25.6 MiB), including its 1,240-byte envelope, under the standard 32 MiB blob
cap. A 50 MiB blob needs both a blob cap of at least 50 MiB and a request
budget of at least **262,145,244 bytes** for that same ref. The receipt reports
`ExceedsBudget` under the standard policy while still accepting the put.

Choose deployment budgets before serving. Lowering a recovery read budget
can refuse previously stored heads; repair the deployment policy before
redriving them. Public `standard` constructors below state the existing
presets; required host choices have no implicit default. Counts and byte
budgets without a smaller safety ceiling stop at their Rust integer's or
platform's addressable range. Raising a parser byte allowance also requires
raising the independent worker source and address-space allowances.

H denotes host admission or retention; S denotes a host setting with a hard
safety ceiling; I denotes lossless mechanics or a semantic representation
contract. New recursive settings apply at deployment or reader construction,
not as saved guest state. The table includes existing configurable budgets as
well as the newly exposed ones. The source inventory found no 12 MB attachment
cap; the workbench's upload preset is 1 MiB.

| Class | Limit / source family | Default | Host configuration | Maximum / reason |
|---|---|---|---|---|
| H | Attachment put admission (`core-store/attachments`) | Unbounded | `LashCoreBuilder::data_retention(...).attachments.max_attachment_bytes` or store builder | `u64`; `None` opts out |
| H | Attachment byte delivery | 32 MiB/blob, 128 MiB/request | `DataRetention.attachments.read.max_blob_bytes`, `.max_request_bytes` | `u64` |
| H | Attachment pending upload lifetime | 24 h | `DataRetention.attachments.upload_expiry_ms` | Representable duration |
| H | Retained tool output / witness | 64 KiB inline, 4 KiB witness | `DataRetention.attachments.output_retention.inline_limit_bytes`, `output_retention.witness_bytes` | Addressable bytes |
| H/S | Retained value descriptor | Depth 4, fields 32, key bytes 64, nodes 96 | `OutputRetentionPolicy.shape: ValueShapeLimits` | Depth 128 protects recursive walks; other cuts addressable |
| H | Attachment delete attempts | 5 | `AttachmentReclamationPolicy::with_delete_attempt_limit` | `u32` |
| H | Attachment reclamation/reconciliation retry | 64 attempts, 8 immediate yields, 2–250 ms backoff | `AttachmentReclamationRetryPolicy` | Representable counts/durations |
| H | Provider file cache | 1024 entries, 24 h | `ProviderFileCacheLimits` | Addressable counts/durations; eviction reuploads |
| H/S | Signed attachment URL horizon | Caller-declared | `DeliveryLimits.valid_through_ms`, `LashCoreBuilder::delivery_fetch_horizon` | S3 7 days, external signing contract; signer refuses impossible horizons |
| H | Commit nodes / encoded bytes | 512 / 64 MiB | `LashCoreBuilder::commit_budget(CommitBudget)` | Bounded nonzero `usize` or explicitly unbounded |
| H | Commit admission queue | Required host policy | `CommitAdmissionPolicy` | Nonzero counts and representable duration |
| H/S | Plugin state keys | 128 bytes | `RuntimeDataLimits.plugin_state.key_bytes: NameByteLimit<128>` | 128: stored key identity and canonical encoding |
| H | Plugin state value / namespace / session | 32 KiB / 128 KiB / 6 MiB | `RuntimeDataLimits.plugin_state.{max_value_bytes,max_namespace_bytes,max_session_bytes}` | Addressable bytes |
| H | State-command count / encoded batch | 64 / 144 KiB | `RuntimeDataLimits.plugin_state.{max_commands,max_encoded_bytes}` | Addressable counts/bytes |
| H/S | Hook, prompt section and wrapper keys | 64 bytes | `HookKey::with_limit`, `PromptSectionKey::with_limit`, `PromptWrapperKey::with_limit`, `NameByteLimit<64>` | 64: stable stored names |
| H/S | Tool intents | 32 total, 16/kind, 64 KiB canonical declaration | `RuntimeDataLimits.tool_intents: ToolIntentLimits` via `LashCoreBuilder::data_limits` | Count `u32::MAX` preserves durable intent ordinals; other counts/bytes addressable |
| H/S | TypeScript cell and parsed workflow source | 64 KiB; nesting 28 | `SourceLimits`, `lower_with_limits` (cell or module mode), `HostArtifacts::import_workflow` with `WorkflowSourceLimits`, `Parser::with_limits`, `WorkerTuning.typescript_source_*` | 128 KiB / 28; measured proportional parser stack stays below 8 GiB; nesting protects conversion/destruction |
| H/S | Python cell and workflow module source | 1 MiB; nesting 96 | Python `SourceLimits`, `lower_with_limits` (cell or module mode), `HostArtifacts::import_workflow` with `WorkflowSourceLimits`, `Python.limits`, worker `python_with_limits`, `WorkerTuning.python_source_*` | Addressable bytes; nesting 96 protects native recursive lowering |
| S | Kernel document / JSON document / binary decode / parked JSON / value / inline-call depth | 64 / 352 / 512 / 512 / 128 / 64 | `KernelDataLimits`; `*_with_limits`, machine `*_with_data_limits`, `WorkerTuning.kernel_data`, `GraphReadPolicy` | Each existing depth is its hard ceiling, protecting recursive native readers/copying/evaluation |
| S | Kernel text and exported TypeScript syntax / result types | 64 nodes; syntax 272 / JSON 352 | Kernel `parse_*_with_limits`, TypeScript `read(source, base, GeneratedSourceLimits)` | Derived from the 64-level document ceiling |
| S | Native JSON and collection conversion/copy | JSON 64; collection values 128 | `KernelDataLimits.document_depth` / `.value_depth` through the machine's native call (`Args::limits`) | 64 / 128, recursive stack safety |
| S | Broker effect JSON conversion | 64 | `datum_{from,to}_json_with_limits(KernelDataLimits)`; host `KernelEffects::outcome` selects effect-result policy | 64, recursive conversion safety |
| H/S | Checkpoint graph reads | Owner 4 GiB; engine and execution 16 GiB; graph depth 64 | `DurableSettings.graph_reads: GraphReadPolicy`, `SqliteStoreSetOptions.graph_reads`, `PoolConfig.graph_reads`, `InProcess::with_read_policy`, explicit `GraphDecodeLimits` | Bytes `u64`; depth 64 and envelope bytes 64 KiB protect canonical graph invariants |
| H/S | Output Type schema admission | 32 levels | `OutputSchemaLimits`, `parse_output_schema_with_limits` | 32, recursive schema walk |
| S | ECMA regex compile admission | 4096 UTF-16 units, 32 group levels | `PatternLimits`, `Engine::with_limits` | Existing 4096 / 32 protect compiler stack and finite indexed program |
| H | Model output tokens / reasoning tokens / stop sequences | Model/profile request choice | `LlmGenerationConfig`, model generation defaults and request overrides | Provider capability contract, typed wire refusal for unsupported settings |
| H | Provider SSE event/line, total, buffered response | 8 MiB / 64 MiB / 16 MiB | `ProviderOptions.{sse_event_bytes,sse_total_bytes,response_body_bytes}` | `u64`, clamped to addressable bytes |
| H | Provider response-start, total-request, retry, throttle time | Attempts and deadlines inherit runtime; backoff 2–10 s + 500 ms jitter; Retry-After 60 s; courtesy 90 s / 8 calls / ≥1 s | `ProviderOptions.reliability`, `LlmTimeouts`, `ProviderReliability` | Finite bounds up to 30 days; `RequestTimeout` explicitly selects inherit, bounded or unbounded |
| H | Stream content blocks | 1024 | `AnthropicStreamLimits`, provider `with_stream_limits` | Addressable count; checked before indexed allocation |
| H | MCP catalog pages / items / bytes | 64 / 4096 / 8 MiB | `McpServerConfig.data_limits: McpDataLimits` | Addressable counts/bytes |
| H | MCP inbound stdio message | 8 MiB | `McpServerConfig.data_limits.stdio_message_bytes` | Addressable bytes |
| H | MCP calls / startup / liveness / reconnect / shutdown | Startup 10 s; call 60 s idle / 600 s total; probe 5 s, disconnect after 3 timeouts, keepalive off; reconnect 500 ms–30 s, unlimited attempts; close 3 s, forced stages 1 s each, scheduling 1 s, child poll 10 ms | `McpServerConfig.{startup_timeout_ms,call_max_total_timeout_ms,call_policy,shutdown_policy}` | Representable milliseconds and validated reconnect/cleanup ordering |
| H | MCP trace propagation | W3C: each `tools/call` sends its `execute_tool` span's `traceparent` and `tracestate` in `params._meta` | `McpServerConfig.trace_propagation: McpTracePropagation` (`w3c`, `off`; `off` sends only the `lash.dev/*` keys and still exports the span) | Two values; no header, allowlist or backend choice |
| H | MCP event transport (SEP-3415) | Off per server; 1,024 buffered notifications, 50 ms reorder settle | `McpServerConfig.events: McpServerEvents::Enabled(McpEventTransportPolicy)`; event listing shares `McpDataLimits.catalog_pages/items/bytes` | Notification overflow returns typed continuity loss; hosts reconnect and reconcile |
| H | Tool body, park, requests, execution allowance | Explicit, no inferred tool default | `ToolDraft` / admitted tool execution and park bounds | Typed bounds; provider/native capabilities still checked |
| H/S | Execution and provider attempts | Recommended model 10 min, control 60 s, stop 2 s; attempts host-declared | `LashCoreBuilder::execution_budgets`, `ProviderAttemptLimits`, tool/model contracts | 30 days / 16 attempts; finite recorded deadline and retry arithmetic |
| H | Turn / no-progress budgets | Required explicit finite or unbounded choice | `SessionSpec` / `TurnBudget` / `NoProgressBudget` | Nonzero addressable count or explicit unbounded |
| H/S | Delta coalescing | 50 ms, 8 KiB, first immediate | `LashCoreBuilder::delta_coalescing(DeltaCoalescing)` | 10 s / 1 MiB; bounded event frame/coalescing latency |
| H | Queued work / age / claimed turn inputs | 64 / 30 s / 64 | `LashCoreBuilder::queued_work_batching(QueuedWorkBatching)` | Nonzero addressable counts/duration |
| H | Prompt sections / wrappers / section bytes / total / render time | 128 / 256 / 32 KiB / 256 KiB / 2000 ms | `PromptLimits` in prompt composition plan | Nonzero `u32` |
| H | Prompt render workers / queued renders | Available cores capped at 8; 1024 jobs | `PromptRenderPoolConfig` | Nonzero addressable counts |
| H | Compact tool examples / chars / schema depth | 2 / 240 / 8 | `ToolPresentationConfig` | Addressable cuts |
| H | Human value/error render cuts | 8000 chars, depth 3, array threshold 10, head 3/tail 2, stack head 3/tail 1 | `RenderParams` | Addressable counts; omitted data remains in source value |
| H | Runtime tool value / raw error text | 16 Ki chars / 4000 chars | `RuntimeOutputCuts`, `LashCoreBuilder::output_cuts` | Addressable character counts |
| H/S | VM wire frame / decode depth / nodes / allocation | 4 MiB / 128 / 100000 / 64 MiB | `ProtocolBounds.decode: vm_protocol::DecodeLimits` | Frame `u32`, depth `u32`, charges `u64`; encoded address-space limits |
| H | VM state / effect input / source / silence watchdog | 2 MiB / 1 MiB / 64 KiB / 5 s | `PoolConfig.protocol: ProtocolBounds` | Wire/addressable sizes and representable duration |
| H | VM run charge / memory / call depth / live tasks / park requests / join members | 50M / 64 MiB / 10000 / 1024 / 256 / 1024 | `PoolConfig.run_bounds` is the ceiling the pool admits; each run states its own `RunBounds` (RLM copies `pool.run_bounds.call_depth` into each cell; runtime hosts use `KernelRecordedSettings::new(dialect, bounds)`) | Typed `u64` / `u32` bounds; a run asking past the ceiling is refused before it starts (`RunRefusal::AboveCeiling { bound, requested, ceiling }`), never clamped |
| H | VM workers / queued items / bytes / restarts | 1–4 / 2 / 8 MiB / 8 per 60 s | `PoolConfig` | Addressable counts/bytes/durations |
| H | VM checkout / compute / serialization / cancel / CPU / attempts | 5 s / 30 s / 5 s / 100 ms / 10 s / 3 | `PoolConfig.deadlines: Deadlines` | Representable durations / `u32` |
| H | VM worker address space | 4 GiB | `PoolConfig.confinement: WorkerConfinement` | Platform rlimit; must exceed parser reservation |
| H | VM worker resident memory / reclaim threshold / CPU share | None (no cgroup placement) | `PoolConfig.cgroup: Option<WorkerCgroup>` (`memory_max_bytes`, `memory_high_bytes`, `cpu: WorkerCpuShare`) | A cgroup v2 subtree the host delegates; each worker gets its own cgroup there. The pool refuses to start when the subtree cannot hold it (`PoolFault::Cgroup`); a worker past `memory.max` ends its run with `WorkerLimit::Memory`. The address-space ceiling holds either way |
| H | VM pool memory budget | None | `PoolConfig.memory_budget_bytes` | Workers × (cgroup `memory.max`, else address space, plus page budgets) + staging quota (`PoolConfig::committed_memory`); no worker starts past it, and a checkout past it meets the queue and checkout faults. Below one worker it is `InvalidConfiguration` |
| H | VM parser stack / parent wait / ingress / slice | 8 MiB + 40000/source byte / 24 h / 16 KiB / 1M charge | `WorkerTuning` | Checked arithmetic; inbound at least frame header; finite positive values |
| H/S | VM diagnostic detail | 512 UTF-8 bytes | `WorkerTuning.diagnostic_bytes: DetailLimit`, `FrameCodec::with_detail_limit`, `Detail::with_limit` | 512 ensures typed refusals fit the minimal frame |
| H | VM heap page residency: clean / dirty / pinned | 64 / 32 / 8 MiB | `WorkerTuning.pages: PageBudgets` | Resident bytes, never guest memory; passing one is a host outcome (`HeapFault::Exhausted`), and residency never changes a verdict or a charge |
| H | VM page staging per activation | 1 GiB, under the system temporary directory | `PoolConfig.{staging_quota_bytes,staging_dir}` | Passing the quota is a host outcome (`StagingExhausted`); a dead parent's spools are deleted when the next one opens the directory |
| S | VM page exchange batch | 64 pages / 4 MiB | `lash_vm_protocol::{PAGE_BATCH_PAGES,PAGE_BATCH_BYTES}` | Fixed protocol bounds (`D-CAP`) |
| S | Root lease / activation page span | 60 s, renewed at a third / 1 h | `lash_vm_broker::kernel::run::PagePlan.{lease_ttl,span}`, set by lash's composition | An expired lease reads nothing (`LeaseLost`) and fences the activation |
| S | Memory refinement trace | 8 MiB trace memory (at most 64 MiB); 100 M work units | The census's `TraceLimits`; lash's composition takes the defaults | Exceeding either is `HostResource::TraceBudget` or `HostWork`, infrastructure, never `Bound::Memory` |
| H | Durable page retirement: sweep period / step nodes / steps per sweep / backlog | 1 s / 256 / 16 / 10 000 holds | `DurableSettings.retirement: RetirementSettings`; PostgreSQL host config `retirement` | Hard maxima 4 096 nodes, 256 steps, 1 000 000 holds (`AboveMaximum`) |
| H | Session revisions / transcript retention | Until GC | `DataRetention.session_revisions`, session retention policy, history queries | Explicit retention choice / caller page limits |
| H | Session replay events / age / sessions / total bytes | 2048 / 120 s / 4096 / 64 MiB | `InMemoryLiveReplayStoreConfig` via `LashCoreBuilder::live_replay_store` | Addressable counts/bytes/durations |
| H | Process replay events / age / bytes/process / processes / total bytes | 2048 / 120 s / 8 MiB / 4096 / 64 MiB | `InMemoryProcessReplayStoreConfig` via `LashCoreBuilder::process_replay_store` | Addressable counts/bytes/durations |
| H | Observation expiry / publisher batch / process tail / dedup / effect-fold / bridge | 64 / 32 / 32 / 4096 per class / 64×256 / 256 | `LashCoreBuilder::observation_work_limits(ObservationWorkLimits)` | Addressable counts; page sizes obey store's paginated read contract |
| H | Provisional language session ingress / bytes/class / process rounds | 256 / 4 MiB / 2 | `ObservationWorkLimits.language_{session_events,bytes,process_rounds}` | Addressable counts/bytes; overflow explicitly retires provisional continuity |
| H | Trace held records / links / extension spans / events per span | 256 / 64 / 256 / 64 | `TraceLimits` via `LashCoreBuilder::trace_limits` | 4096 / 128 / 4096 / 256; each refused typed (`TracePolicyError::Range`) |
| H | Trace attributes per span / value / key / name / span contributions | 128 / 4 KiB / 128 B / 1 KiB / 256 KiB | `TraceLimits` | 1024 / 64 KiB / 1 KiB / 16 KiB / 8 MiB; `attributes_per_span` at least every kind's `attribute_reserve`, contributions share what the reserve leaves (`TracePolicyError::Relation`) |
| H | Trace extension bytes / tool-span annotations / references / reference bytes / publication hold | 1 MiB / 16 / 16 / 64 KiB / 256 records, 8 MiB | `TraceLimits` | 16 MiB / 64 / 64 / 1 MiB / 4096, 64 MiB; annotations and references at most the span's remaining slots |
| H | Provider body captured under extended model detail | 2048 B | `ContentLimits` via `LashCoreBuilder::content_limits` | 8 MiB |
| H | Attempt stream / diff value / diff paths / failure output / process effect occurrences | 256 KiB / 2048 B / 8 / 64 KiB / 64 | `EvidenceLimits` via `LashCoreBuilder::evidence_limits` | 16 MiB / 1 MiB / 4096 / 8 MiB / 4096 (the process pins its value); `EvidenceLimitsError::Range` |
| H | Diagnostic error chars / plugin-state warning | 4000 / 4 MiB (or off) | `DiagnosticLimits` via `LashCoreBuilder::diagnostic_limits` | 1 MiB chars / 64 MiB |
| H | Composition generations / sessions | 8 / 1024 | `CompositionCacheLimits` via `LashCoreBuilder::composition_cache_limits` | 4096 / 65536 |
| H/S | W3C tracestate members / chars / key / tenant / system / value | 32 / 512 / 256 / 241 / 14 / 256 | `TraceStateLimits::new`, `TraceCarrier::parse_w3c_with` | Each default is its W3C maximum; above it is refused typed |
| H | Flight recorder capacity / slow threshold / interval | 256 / 5 s / 60 s | `FlightRecorderSettings::new` | 16384 / 24 h / 24 h |
| H | Workflow overlay history / mismatches | 256 / 64 | `WorkflowExecutionOverlayAccumulator::new`, `.with_mismatch_limit` | Addressable counts |
| H | Catalogue preview modules / call names | 100 / 50 | `CataloguePreviewOptions` | Addressable counts |
| H/S | Charge-safety retry count | Host-declared policy | `ChargeSafetyPolicy`, session/core config | 5 unsafe retries; bounded provider billing exposure |
| H | Durable lease TTL / heartbeat / self-stop / reap / poll / backoff / startup / shutdown | 15 s / 3 s / 10 s / 2 s / 250 ms / 25 ms / 2 s / 2 s | `DurableSettings.lease: LeaseSettings` | Representable durations with lease ordering validation |
| H | Durable claim / active / activation loop / group commit / snapshot / cascade | 16 / 256 / 8 / 64 members and 5 ms / 1M fuel / 256 | `DurableSettings` | Typed representable counts/durations |
| H | Owner idle retention and resident count | 1 hour / 10,000 | `DurableSettings.owner_retention` | Nonzero session/process count; zero idle releases immediately |
| H | Host item ingress bytes | 64 MiB | `DurableSettings.max_ingress_bytes` | Nonzero and at most the runtime commit byte budget; checked before durable enqueue |
| H | SQLite busy / connection and WAL working policies | Busy 15 s; role-specific presets | `SqliteConnectionPolicy`, store/node working config | Platform/typed ranges, timeout checked |
| H/S | PostgreSQL connection prefix / replay schema names | 32 / 48 bytes | `PostgresHostConfig.name_limits: PostgresNameLimits` | 32 / 48 reserve suffix space within PostgreSQL's 63-byte identifiers |
| H/S | PostgreSQL roles / SQL deadlines / retries / replay / maintenance | Role presets in `docs/operations/postgres.md`; live replay 2048/120 s/8 MiB; process replay 2048/120 s/8 MiB/4096/256 MiB | `PostgresHostConfig.{roles,guards,retry,signals,live_replay,process_replay,maintenance,deployment}` | Server timeouts `i32::MAX` ms; replay events ≤1M, batch ≤65536, per-owner bytes ≤1 GiB, age ≤24 h, aggregate process replay ≤1 TiB; other validated role ranges documented there |
| H | Workbench upload / attachment name / retrieval / HTTP request body | 1 MiB / 200 chars / 32 MiB / 2 MiB | `AGENT_WORKBENCH_DATA_LIMITS` JSON typed as `WorkbenchDataLimits` | Addressable sizes; application deployment policy |
| I | Graph envelope / root / inline token / leaf / fanout | 64 KiB / 4 KiB / 256 B / 65520 B / format fanout | Canonical chunking; caller read budgets may narrow envelopes | Larger payloads stream across nodes; no total payload refusal |
| I | SQL wake / replay notification payload | 7900 bytes | No host knob needed | Oversize uses authoritative polling / invalidation notification, data stays in store |
| I | Process registry/roster pages and attachment root scan lookahead | 256 + 1 lookahead | Caller page size, bounded page with continuation | No truncation of the complete scan |
| I | Failed-settlement uncharged receipt | 1024 encoded bytes | Accounting exemption only | Larger refusal is charged normally, not dropped |
| I | Regex register limits / opcode chunks / optimization depth | 65535 groups/loops; 16-byte instructions; duplicate depth 100 | Outer `PatternLimits` covers reachable admission | Register caps unreachable under 4096 units; optimizer declines optimization, keeps original program |
| I | Type inference fixed-point rounds | 32 | Internal conservative inference | No source/result refusal; unknown facts remain conservative |
| I | Provider model name/hash shortening and HTTP error summary | MCP 64 chars; error fallback 200 chars | Native names retained in dispatch map; raw HTTP error retained in envelope | Lossless mapping or presentation alongside original data |
| I | Read/cache buffers, reservation steps, batching and cooperative slices | Various | Existing cache/working configs where useful | Evict/reload, paginate or yield; data remains recoverable |
| I | Numeric/time domains, varint width, Unicode, schema/format version | ECMAScript TimeClip 8.64e15 ms; exact f64 integers 2^53; u64 varint 10 bytes | Semantic/representation contracts | Changing these changes the language or stored representation, not an operational budget |
| I | Test generators, conformance/performance witnesses, diagnostic sampling counters | Harness-specific | Harness arguments / explicit measurement selection | No production host/user payload admission |

## Guest call depth

Guest recursion uses `PoolConfig.run_bounds.call_depth`, whose standard value
is `Bounds::DEFAULT_CALL_DEPTH` (10,000). Set it before constructing the worker
pool. RLM takes this value from its pool for every cell; process hosts persist
it through `KernelRecordedSettings::new`. A direct worker client states a
`RunBounds.call_depth` no greater than its pool ceiling. Zero is refused.
Calls into helpers count toward this limit. All guest frames live on the heap
and are charged to the memory meter, so the memory bound can end a deep run
first. The register executor's 200-frame reservation is an internal batch;
when it fills, execution continues on the general path. A depth refusal is a
noncatchable typed `CallDepth` end, with the nearest guest source site carried
through the worker wire and recorded end. RLM projects it through the checked
source map to the source line and column.

Measured stack and heap evidence is in
[Guest call depth](../perf/instruments/guest-call-depth.md).

## MCP event transport and host delivery

With the `mcp` feature, `McpPluginFactory::events()` returns a
`lash::mcp::events::McpEventClient`. Enable each server with
`McpServerConfig::with_events(McpEventTransportPolicy::default())`.
The client provides `list`, `poll` and `open`; an open `McpEventStream`
provides `read_window` and `close`. Requests, occurrences, opaque cursors,
refusals and continuity loss are typed facade data. A retention gap is
available through `continuity_loss()`; notification overflow returns
`McpEventsCallError::ContinuityLost` and poisons that connection's windows
until reconnection. Do not commit progress from a failed window.

Lash registers no event subscription engine or model subscription tools.
Hosts own durable subscription registries, enabled state, authorization,
poll/stream loops, scheduling, mapping, routing, gap reconciliation and
retention. Commit occurrences and their cursor in host storage before
acknowledging progress. Retry delivery through `send().id(TurnId)` or keyed
process start with unchanged content and a key derived from the source,
subscription generation and occurrence identity. Retain delivery evidence
for the source's redelivery horizon and coordinate it with Lash pruning.
See [the reference host](../../examples/mcp-events/README.md) and
[ADR 0137](../adr/0137-the-host-owns-events-routing-and-scheduling.md).
