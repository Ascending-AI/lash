# Hosting lash 1.0 on the durable substrate

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
    .projection_provider(Arc::new(TicketProvider::new(client)))
    .build()?;
// Each policy value below is chosen by the host.
let core = lash::LashCore::builder(backend)
    // Example host limits; measure and select them for the deployment.
    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
    .tool_source_policy(lash::tools::ToolSourcePolicy::Require)
    .execution_budgets(budgets)
    .delta_coalescing(lash::DeltaCoalescing::recommended())
    .data_retention(lash::DataRetention {
        session_revisions: lash::Retention::HeadOnly,
        ..lash::DataRetention::standard()
    })
    // providers, plugins, models, tracing ...
    .build(lash::persistence::LeaseOwnerIdentity::opaque(node_name, boot_id))?;
```

### Required choices before serving

The core requires all six policy inputs below. A named preset is a host's
explicit choice of values, not an implicit policy or a measured deployment
recommendation. `build` refuses a missing core choice. The same applies to
session, model, RLM and store choices.

The example selects a 1,024-token action reserve for queued work and refuses
missing persisted tool sources. The reserve is advisory data for a custom
drain policy; the shipped drain modes do no token arithmetic. Tool and engine
body bounds and park bounds belong to their execution contracts, separately
from the shared model, control and provider budgets.

| Boundary | Required host choices | Current owner |
| --- | --- | --- |
| Core | `commit_budget`, `queued_work_batching`, `tool_source_policy`, `execution_budgets`, `delta_coalescing`, `data_retention` on `LashCoreBuilder` | [Builder](../../crates/lash/src/core.rs) and [required-policy validation](../../crates/lash/src/core/runtime_host_config.rs). |
| Session creation | `SessionSpec::model`, `turn_budget`, `max_tool_calls`, `no_progress_budget`; supply tool authority through `SessionCreation::root(tool_access, spec)` or `child_of(tool_access, parent, spec)` | [Session spec](../../crates/lash-core-execution/src/session_model/mod.rs) and [creation](../../crates/lash/src/session.rs). A child records supplied config; only a fork clones. |
| Recorded model | Wire model, nonzero context window and `CacheRetention` in `LlmProfileMetadata`; choose capability and request defaults for the route | [Model metadata](../../crates/lash-sansio/src/llm_profile.rs). |
| RLM, when installed | `RlmProtocolPluginConfig::builder()` requires `instruction_limit`, `memory_limit` and `channel` before `build()` | [Typed builder](../../crates/lash-protocol-rlm/src/plugin/config.rs). |
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

`LashCore::shutdown` resigns the core's recovery leadership, stops its node,
then shuts down its plugin factories. A node stops with one of the runner's
reasons (`lash::durable::runner::Stopped`):

- `Requested`: the host asked. The node releases its actors, and another node
  claims them within a claim poll. The runbook measured a 7 ms handover.
- `LeaseLost`: another node reaped this one, or a newer boot of its name
  replaced it.
- `Unrenewed`: renewals failed for `self_stop_after`, so it stopped before
  anyone could reap it.
- `Drained`: it drained by release ([below](#drain-by-release)).

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
and resume each from its committed rows: nothing is re-run that committed,
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

**`Once` holds across a database failover only if acknowledged commits
survive promotion.** The host chooses its topology. Lash runs no startup check
and offers no mode switch
([ADR 0132 §13](../adr/0132-durability-is-state-first-over-the-lash-store.md#13-the-postgresql-durability-assumption)).

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
`core.processes().start(request, core.effect_host()).await` or send input
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
`program_identity`, `creation_config` (recorded on the process row and handed
back to engine steps as `EngineStepRun::engine_config`), `start_artifacts`,
`end_artifact_referrer`, `acquire_engine_artifact`, `check_args` and `resolve`.
`check_args` checks supplied arguments against an authoritative signature;
engines without a checkable signature return `ArgsMismatch::UnsupportedSignature`.
Hosts invoke it through
`core.process_definitions().check_args(&definition, &args, mode).await`, with
`ArgsMode::Partial` or `ArgsMode::Complete`. Partial checks every supplied value
and rejects undeclared names; Complete also requires every declared argument.
The facade reads the retained definition, verifies its signature claim, and
checks arguments without starting a process or acquiring a lasting pin.

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
`process.resumed` when the wait ends. A host that lost the key, or restarted
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

    // program_identity, creation_config, start_artifacts,
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
| `control_phase` | 60 s | One admission or checkpoint phase, all its checks together. |
| `stop_grace` | 2 s | Spent once after a stretch ends at its limit or on cancel, to collect evidence. |
| `provider` | see below | `ProviderAttemptLimits` |
| `agent_frame_switch_limit` | 16 | A chain of agent frame switches; the follow-on at this depth stops with `AgentFrameSwitchLimit` before calling the model. |

`ProviderAttemptLimits::new(per_request, response_start, chunk_idle,
max_attempts)` states the attempt limits; `ProviderAttemptLimits::recommended()`
is 5 min per request, 2 min to the response start, 2 min of chunk silence and
4 attempts. Each bound is clipped to the call's
remaining `model_total`.

`new` refuses with `ExecutionBudgetsError`:

- `OutOfRange`: a bound below 1 ms or above `MAX_EXECUTION_BUDGET` (30 days);
- `DefaultExceedsCeiling`: `provider.per_request` above
  `model_total`, or a provider sub-bound above `per_request`;
- `SumOverflows`: a stretch plus its grace exceeds the maximum;
- `UnboundedRetry`: `max_attempts` is 0 or above `MAX_PROVIDER_ATTEMPTS` (16).

Limits are recorded before their work starts and never refreshed. A nested
stretch takes `min(own, enclosing remaining)`. An expired limit found on load
settles at once.

### Tool bounds are host declarations

Every tool definition calls `with_execution(Duration)` to supply its required
body bound. The manifest records `execution`. A tool that can defer also
calls `with_park(ParkBound)`, either `Within(Duration)` or `UntilScopeEnd`; a
non-deferring tool supplies none. Registration refuses a missing bound as
`MissingBound { tool, bound }`, or
a park on a non-deferring tool as `ParkWithoutDeferral { tool }`. Admission
of an ungated manifest refuses as `ToolAdmissionRefusal::Bounds`.
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

## 7. Projection providers

A VM never holds a live host object. A projection value is plain data:
`lash_vm::ProjectedValue::resource(name, type_name, ResourceRef)` (in the
facade, `lash::vm::ir`). It snapshots with the heap and pins no node.

- **`ResourceRef { projection, id, revision }`** names the provider's type,
  the resource, and optionally a revision or snapshot id. A provider that must
  answer identically after a failover, when another node reads the same value,
  sets `revision` and answers reads at that revision.
- **`ProjectionProvider`** answers one `ProjectionType`:
  - `projection_type()`;
  - `read(resource, ProjectedReadRequest)` returns
    `Result<Option<ProjectedReadResponse>, ProjectionError>`;
  - `read_range(resource, requests)` answers a batch in order, in one IPC
    frame. Use it for hot loops.

  `Ok(None)` means the provider does not answer that request at all, which is
  not the same as "no value". No method has a default body.
- **Registration.** Register providers on `DurableBackendBuilder::projection_provider`,
  one per type, as tools are registered in the catalog. A read dispatches to
  the provider on whichever node runs the actor. A value whose type has no
  provider is refused with `ProjectionRefusal::NoProvider`, never a
  placeholder.
- **Purity.** Reads are pure and `Repeatable`, and they are not journaled. A
  read before a snapshot is already in the heap; a read after a restore reads
  again. A provider must not write, and must tolerate reading the same thing
  twice.
- **`history`** is lash's own provider over the session transcript, pinned at
  a revision in its `ResourceRef` (`lash_protocol_rlm::HISTORY_PROJECTION`).
  A host provider of the `history` type is refused at build.

The live-object export registry, exported host descriptors and the
unavailable-after-restore placeholder are deleted. A host object that used to
be exported (a sandbox handle, a document, a ticket) becomes a `ResourceRef`
naming it plus a provider that reads it.

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
lock attempts and polling, migration polling, checkpoint/graph chunks and
event-release paging. `SqliteOperationalSettings::standard()` documents every
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
| `idle_evict` | 60 s | How long an owner keeps an idle actor hot before releasing it. |
| `activation_loop_budget` | 8 | Claims in a row without progress before an actor parks with `ActivationLoop`. |
| `group_commit` | 64 members, 5 ms | Finished round members commit in batches (`GroupCommit { max_members, window }`). The window must be shorter than `lease.claim_poll`. |
| `snapshot_every_fuel` | 1 000 000 | VM fuel spent without an effect after which a quiet point snapshots anyway. |
| `cascade_batch` | 256 | How many `Until` children one cascade transaction marks. |
| `notifier` | `AfterCommit` | `AfterCommit` publishes wake hints and holds the liveness lock, on PostgreSQL and on a SQLite file; `PollOnly` relies on the claim poll alone. |

Counts must be at least 1 (`Zero`), durations at least 1 ms
(`BelowResolution`), plus `ClaimBeyondCapacity`, `GroupWindowNotBeforePoll`
and the lease rules (`Lease(LeaseConfigError)`).

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
`LashCore::processes().redrive(process_id, requester)` (`lash::process::Processes`)
clears a process's park and activation count so its engine runs again from
committed state; it answers whether the process was parked. Cancelling a
parked process ends it without running its engine.

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

The process lifecycle log has a closed vocabulary: started, waiting with
`call_id` and `tool_id`, resumed, effect outcome, effect omissions, cancel
requested, observer added and removed, external reference set, and completed,
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
[response context](../../crates/lash-sansio/src/llm/response_context.rs) and
`lash::provider` in [facade rustdoc](../../crates/lash/src/lib.rs).

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
