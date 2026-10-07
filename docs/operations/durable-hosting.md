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

## What has not landed yet

One host-facing part is still open.

- **Processes on the core's node** (FIG-5216). A `LashCore` runs one node,
  and that node serves its backend's session actors only: it claims no
  process actor. A process that a session or the host starts is written and
  woken, but nothing served through the facade runs it. A node that runs
  processes is served today by `lash_core::runtime::durable::node::serve`
  with a `ProcessActivation` over the host's `ProcessSteps`, as the
  [`lash-postgres-workers` runbook](../../runbooks/lash-postgres-workers/README.md)
  does. The facade does not re-export them. [§4](#4-host-process-engines)
  describes the end state.

## 1. What a host runs

### No engine server

A 1.0 host runs no workflow-engine server, registers no handlers and pins no
engine SDK. It does not configure an ingress or admin URL, and it exposes no
handler endpoint.

A host runs **lash nodes**. A node is one host process that serves lash's
runner over a store set:

- **PostgreSQL:** any number of nodes over one database;
- **SQLite:** exactly one node over one database file ([§3](#3-sqlite-is-one-file)).

Work enters through the facade (`LashSession::send`, process starts,
signals, trigger occurrences, `Completions::resolve`). Each write commits a
mailbox row and wakes the actor that owns the work. Whichever node owns that
actor runs it. A host never drives a turn itself: there is no handler, effect
controller or work driver for it to call.

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
    .config(DurableSettings::default())
    .process_engine(Arc::new(CiEngine))
    .projection_provider(Arc::new(TicketProvider::new(client)))
    .build()?;
let core = lash::LashCore::builder(backend)
    .execution_budgets(budgets)
    // providers, plugins, models, tracing ...
    .build(lash::persistence::LeaseOwnerIdentity::opaque(node_name, boot_id))?;
```

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

The reaper deletes an expired node row and, in the same statement, releases
its actors with an **epoch bump**. Every owner transaction begins by reading
its actor's epoch under lock. A node that lost its actors therefore cannot
commit for them, even while it still runs: its write fails `OwnershipLost`,
rolls back, and the node drops the actor. Lease expiry is never checked at
commit time.

On PostgreSQL with the `AfterCommit` notifier ([§2](#2-postgresql-topology)), a
crashed node is also detected through its listener session: the other nodes
reap it within about one claim poll instead of waiting out the lease. The
failover runbook measured 257 ms with the lock and 15 963 ms without it
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

Every store call the runner makes races the host's stop and the self-stop
deadline, so a heartbeat that hangs still stops the node at `self_stop_after`,
and its activations with it. Bound the store's connect and statement timeouts
anyway, so a hung call ends.

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

Each node opens three kinds of connection:

- the shared pool (`PostgresStoreConfig::max_connections`);
- four reserved connections for lease, terminal and cancel commits, so a burst
  of ordinary commits cannot starve the heartbeat;
- one listener connection, which receives wake hints (`LISTEN`) and holds the
  node's liveness lock, a session advisory lock.

Budget `max_connections + 5` server connections per node.

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
the process registry, the trigger store and the durability core, so one
transaction commits rows of every family
([Deploying and upgrading](deploying-and-upgrading.md#choose-the-deployment-shape)).

- Open it with `lash::sqlite::SqliteStoreSet::open(path)`. Tests use
  `SqliteStoreSet::memory()`.
- **One process per file.** Exactly one node serves it. SQLite has no
  notification channel or liveness lock, so wakes stay in process.
- **No shared multi-node SQLite.** Two processes over one file, or a file on a
  network share, are not a supported deployment. Use PostgreSQL for more than
  one node.
- A path that is a directory in the retired three-file layout is refused
  `retired_sqlite_layout`. Formats reset at 1.0; nothing migrates it.
- Durable instants come from the injected clock.

## 4. Host process engines

A host `ProcessEngine` (`lash::plugins::ProcessEngine`) is an explicit state
machine
([ADR 0132 §10](../adr/0132-durability-is-state-first-over-the-lash-store.md#10-host-process-engines-are-state-machines)).
Lash calls `advance(state, event)` with the process's committed state and one
event. The engine answers its next state and one action. The new state and
the action's admission commit in one `process.advance` transaction before the
action runs.

### The contract

- **`advance` is effect-free and deterministic.** It is synchronous. The same
  `EngineState` and `EngineEvent` give the same answer. It calls no service,
  reads no clock and writes nothing. If the commit after it fails, lash calls
  `advance` again with the same state and event; that is recomputation, not
  replay.
- **There is no `run`.** There is no opaque run method, no `await_terminal`
  and no re-entry from the top. No trait method has a default body.
- **Effects are actions.** `EngineAction` has eight variants:

  | Action | Lash does | `advance` next receives |
  | --- | --- | --- |
  | `Steps(Vec<StepRequest>)` | Admits every step (started rows) in the transaction, then runs them. Non-empty. | One `StepSettled { step, outcome }` per step |
  | `PinKey { name, kind, deadline }` | Mints a host-resolvable wait (`HostWaitKind::ToolCompletion` or `Custom`) and its key. | `KeyPinned { name, key }` at once |
  | `AwaitExternal { name }` | Waits on a key pinned earlier. | `ExternalResolved { name, resolution }` or `ExternalTimedOut { name }` |
  | `AwaitProcess { process, deadline }` | Waits on another process's terminal. | `ProcessEnded { process, outcome }` or `ProcessWaitTimedOut { process }` |
  | `Sleep { until }` | Sets a durable due time. | `Woke` |
  | `Idle` | Nothing, with no deadline. | The next mailbox event |
  | `Emit { event_type, payload }` | Appends one process event in the same transaction. | `Emitted` at once |
  | `Terminal(ProcessOutcome)` | Ends the process. | Nothing |

  `Signal(ProcessSignal)` and `Cancelled { origin, grace_until }` can arrive
  whenever the process is waiting. `Started { payload }` is the first event.
- **Steps.** `StepRequest::Tool { step, tool, input }` names a catalog tool.
  Its declaration's `ExecutionPolicy` (`Once` or `Repeatable`) and a limit
  within the tool ceiling are pinned at admission. Its body runs through the
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
- **Waits are bounded.** `PinKey` and `AwaitProcess` take an optional deadline.
  With none, the wait gets `wait_default`; above `wait_ceiling` the action is
  refused. Today the process activation reads these from
  `ExecutionBudgets::default()` (1 h, 24 h), not from the core's configured
  budgets. An `AwaitProcess` races the awaiter's own cancel mail, so a cycle of
  processes awaiting each other stays cancellable.
- **Refused actions end the process.** A `Steps` with no request, a step name
  already in flight, an `AwaitExternal` for a key never pinned, a deadline
  above the ceiling or a refused step admission ends the process with a
  `process_action_refused` failure.
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
`end_artifact_referrer`, `acquire_engine_artifact` and `resolve`.

### Worked example: a 40-minute CI suite

A tool `run_full_suite` runs a CI suite that takes about 40 minutes. It is far
above the inline ceiling, so it is declared as a process tool that starts a
`ci` process. The process submits the job, then waits for CI to call back with
the key.

```text
TURN    run_full_suite admitted -> its body starts process P (kind "ci") -> durable wait on P
P       Started            -> PinKey { name: "done", kind: Custom, deadline: 45 min }
        KeyPinned { key }  -> Steps([Tool { step: "submit", tool: "ci.submit",
                                            input: { suite, key } }])     ci.submit is Once
        StepSettled(Completed) -> AwaitExternal { name: "done" }           P releases as waiting
CI      40 min later: Completions::resolve(key, Resolution::Ok(report)), retried until answered
P       ExternalResolved   -> Terminal(success: report)
TURN    the wait on P resolves -> model -> commit
```

```rust,ignore
use std::time::Duration;
use lash::plugins::{
    EngineAction, EngineEvent, EngineState, EngineStateFormat, HostWaitKind, KeyName,
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
                    kind: HostWaitKind::Custom,
                    deadline: Some(Duration::from_secs(45 * 60)),
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

Lash's own lashlang engine follows the same contract: its `advance` answers
`Steps([vm_run])`, and the VM runs only inside that engine step. A host does
not register it: the RLM protocol plugin factory contributes it, with a
`LashlangRunSettingsRecorder` that records each process's surface at creation.

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

- `outstanding(session_id)` lists the session's unresolved host-resolvable
  keys (`lash::durable::PinnedKey`), read from their rows.
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

Hosts resolve only the `tool_completion` and `custom` kinds. Signals, turn
cancellation, process terminals, timers and child-session ends have their own
admission paths, and a host resolution of them answers `ReservedKind`. Lash
applies no authorization of its own: authenticate and authorize the caller
before resolving on its behalf
([ADR 0014](../adr/0014-operational-policy-stays-with-the-host.md)).
A webhook that receives callbacks retries until it gets an answer and treats
`AlreadyResolved` as done.

## 6. Execution budgets

`lash::ExecutionBudgets` is the one source of every execution bound. Set it
with `LashCoreBuilder::execution_budgets`. `ExecutionBudgets::new(config)`
validates an `ExecutionBudgetsConfig`:

| Field | Default | Bounds |
| --- | --- | --- |
| `tool_default` | 2 min | One inline tool execution whose manifest declares no duration (`ExpectedExecution::Default`). |
| `tool_ceiling` | 5 min | The longest inline execution a tool may declare. |
| `model_total` | 10 min | One model call, over throttle, backoff and every provider attempt. |
| `control_phase` | 60 s | One admission or checkpoint phase, all its checks together. |
| `stop_grace` | 2 s | Spent once after a stretch ends at its limit or on cancel, to collect evidence. |
| `wait_default` | 1 h | A deferred or external wait with no declared deadline. |
| `wait_ceiling` | 24 h | The longest deferred or external wait. |
| `provider` | see below | `ProviderAttemptLimits` |

`ProviderAttemptLimits::new(per_request, response_start, chunk_idle,
max_attempts)` defaults to 5 min per request, 2 min to the response start,
2 min of chunk silence and 4 attempts. Each bound is clipped to the call's
remaining `model_total`.

`new` refuses with `ExecutionBudgetsError`:

- `OutOfRange`: a bound below 1 ms or above `MAX_EXECUTION_BUDGET` (30 days);
- `DefaultExceedsCeiling`: `tool_default` above `tool_ceiling`,
  `wait_default` above `wait_ceiling`, `provider.per_request` above
  `model_total`, or a provider sub-bound above `per_request`;
- `SumOverflows`: a stretch plus its grace, or `wait_ceiling` plus
  `tool_ceiling`, exceeds the maximum;
- `UnboundedRetry`: `max_attempts` is 0 or above `MAX_PROVIDER_ATTEMPTS` (16).

Limits are recorded before their work starts and never refreshed. A nested
stretch takes `min(own, enclosing remaining)`. An expired limit found on load
settles at once.

### The inline ceiling

A tool that can only finish inline and declares more execution than
`tool_ceiling` is refused at registration with
`RegistrationRefused::InlineBudgetExceedsCeiling { tool, declared, ceiling, hint }`.
Long work takes one of three shapes, each with a bounded inline prefix and a
wait deadline:

- **A process tool:** the declaration's intents include `StartProcess`; the
  tool starts a process (for example the [CI engine](#worked-example-a-40-minute-ci-suite))
  and the call waits on it.
- **An isolated tool:** `ToolDeclaration::isolated`; the call runs on a host
  engine.
- **A Pending tool:** `ToolDeclaration::may_defer`; the call defers and an
  external system resolves it with its completion key.

Nothing is promoted at runtime: the declaration decides.

## 7. Projection providers

A VM never holds a live host object. A projection value is plain data:
`lashlang::ProjectedValue::resource(name, type_name, ResourceRef)` (in the
facade, `lash::rlm::lang`). It snapshots with the heap and pins no node.

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
| `notifier` | `AfterCommit` | `AfterCommit` publishes wake hints and holds the liveness lock on PostgreSQL; `PollOnly` relies on the claim poll alone. |

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
`ProcessParkReason`.

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

**Concurrent signals are unordered.** Signals reach `advance` as
`EngineEvent::Signal` in mailbox order per sender. Two senders racing have no
defined order. A host that needs an order across senders puts a sequence in
the payload and orders in its engine.
