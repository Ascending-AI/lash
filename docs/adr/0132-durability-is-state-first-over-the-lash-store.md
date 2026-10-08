# 0132: Durability is state-first over the lash store: actors, epoch fences, no replay

## Status

Accepted.

## Context

Lash runs agent turns, tool rounds, long processes and one heap VM. An
external journaling workflow engine records them by position and replays
handler code against that journal. The workload does not fit that model: long
tool bodies meet silence timers, suspension is refused while an awaited run
executes, and every kernel change pays for journal determinism (a journal
logic epoch, generation lanes, a replay corpus, forced-replay legs). Such an
engine also makes two sources of durable truth, the SQL stores and the
engine's journal, joined by an obligation relay because a store commit and an
engine send cannot be atomic.

A store-backed replay engine is not the answer either. The SQL replay driver
deleted in commits `4f03596847` and `476264fbea` re-ran async code against
outcomes keyed by `(scope_id, replay_key)`. It kept a second journal-replay
engine and mirrored 293 statements across two dialects.

## Decision

### 1. One durable engine over the lash store

Lash's own runtime is the only durable engine. It persists state through the
lash store: PostgreSQL for nodes on any number of machines, and one SQLite
database file for nodes on one machine, each node a process of its own. Both
run the same engine code and offer one node interface: leases, epoch fences,
takeover, wake hints between nodes and crash detection by liveness lock.
SQLite's limits are scale, one writer at a time, and locality, a local file
on one machine. There is no external engine and no pluggable `EffectEngine`
seam:
`Backend` builds the durable engine directly over its store set.

Engine SQL is written once per dialect, in one module, behind a gate that
refuses engine-table SQL anywhere else. No engine statement is mirrored by
hand across dialects.

Lash runs no OS programs and writes no local files beyond the configured
SQLite database and the lock files beside it.

### 2. The no-replay rule

No code is ever re-run against a recorded history, by position or by key.
Resume loads state and continues from it.

- A committed phase record is state. Resume folds rows into state; the fold
  calls no producer.
- An uncommitted stretch of work is recomputed from the last committed state.
  It never consults an outcome recorded inside that stretch, because none is
  recorded: every effect whose outcome must survive commits as its own phase.
- Orchestration code (shift, turn loop, `RunCoordinator`, a host engine's
  logic) never re-executes to reach a recorded outcome.

The rule is a falsifiable law, checked by the crash matrix of §14. For every commit
label and every cut, after resume on another owner:

- NR-1. Every admitted identity with a committed outcome has executed its
  body exactly once in total.
- NR-2. A `Once` identity started without an outcome executes zero further
  times and records `Interrupted`.
- NR-3. A `Repeatable` identity started without an outcome re-executes at its
  same ordinal; nothing else executes again.
- NR-4. The resume path performs zero outcome lookups on behalf of re-running
  code. Loading is one fold per actor.

The harness counts body executions per admitted identity and outcome lookups
per resume. Any count above its bound fails the law.

### 3. Actors own sessions and processes

- **Actor.** Each session and each process is an actor with one scheduling
  row. The row holds state (`idle`, `ready`, `owned`, `waiting`, `parked`,
  `terminal`), the owning node and incarnation, an epoch, a ready time, the
  earliest durable due time and a mail flag. A process is its own actor, also
  when it is turn-scoped; its lifetime coupling is the cancellation cascade
  of §11.
- **Node liveness.** Each serving node keeps one heartbeat row. A node whose
  heartbeat fails to renew stops itself: it drops every actor and cancels
  every body. A reaper deletes expired node rows and, in the same statement,
  releases their actors with an epoch bump.
- **The epoch is the fence.** A claim and a reap are the only writers of an
  epoch, and both bump it. Every owner write transaction begins by reading the
  actor row under lock and comparing the epoch with its own. A mismatch is
  `OwnershipLost`: the transaction rolls back and the owner drops the actor.
  Lease expiry is not checked at commit; an expired owner that nobody reaped is
  still the only owner.
- **Uniqueness constraints are a second fence.** At most one Run record per
  ordinal and one outcome per admitted identity, so a fence bug cannot commit
  two outcomes.
- **The owner cache is never a grant.** The owner keeps the actor's state in
  memory, keyed by `(actor, epoch)`. Only the owner writes owner-state rows, so
  the cache goes stale only through new mail, which every fence read reports,
  or through lost ownership. On any failed fence the cache is discarded and
  state reloads from rows. It is never patched.
- **`ActorTx` and `MailTx`.** An `ActorTx` is fenced and may write the actor's
  state rows. A `MailTx` is what every other writer gets: it appends to an
  actor's mailbox (inputs, cancel requests, wait resolutions,
  parent-end) and wakes the actor, and has no owner-state writers. The two are
  distinct types, so the compiler enforces the split.
- **Crash loops.** A claim with no phase progress since the previous claim
  counts a failed activation. At the activation budget the actor parks with
  `ActivationLoop`. Progress resets the count, so node restarts during deploys
  do not fail long work.
- **Clock.** Durable instants come from the store clock: the database clock
  on PostgreSQL, the injected clock on SQLite. Live enforcement arms local
  monotonic timers for `expires_at − now`.

Substrate parameters (lease TTL, heartbeat and reap intervals, self-stop
bound, claim poll, idle eviction, activation budget, cancel grace) live in one
typed, validated `DurableConfig` with refusal laws.

### 4. Turns restore from their checkpoint

A turn is a sequence of committed phases of the sans-io `TurnMachine`.
Production restores from `TurnCheckpoint`
(`crates/lash-sansio/src/sansio/machine_state.rs`,
`crates/lash-sansio/src/sansio/turn_machine.rs`) and re-delivers the pending
effect the checkpoint names.

- Admission commits the turn row, binds its inputs and records the turn
  deadline.
- A model call is `Repeatable` generation. Its request is pinned and its
  `model_total` deadline recorded before the first byte is sent. A crash
  re-sends the pinned request as the next attempt while the deadline allows.
- Streaming is not durable. Deltas go to the live replay store; a re-sent call
  retracts what its earlier attempts streamed with one `ModelAttemptReset`
  read back from the live replay, then streams under ids of its own
  ([ADR 0040](0040-retried-model-attempts-retract-live-text-by-correlation.md)),
  so clients discard the abandoned stream and keep their cursors. The
  completed response commits with the next checkpoint and the round's
  admission in one transaction.
- The turn commit is one transaction: the session head compare-and-set,
  terminal evidence, the parent-end of `Until(turn)` processes and pruning of
  the turn's phase rows. The head compare-and-set makes it idempotent.
- A preparation hook recomputes from committed state when its phase did not
  commit, so hooks are repeat-safe. An at-most-once external mutation is an
  admitted `Once` execution, not a hook.

### 5. Tool calls are phase rows

The `RunLedger` records persist as rows keyed by `(owner, run, ordinal)`, and
the fold stays the authority (`crates/lash-core-execution/src/tool_dispatch`).
The execution record splits into a start and an outcome.

- **Started before the body.** A round's admission (membership, pinned policy,
  `ExecutionLimit`s, `WaitDeadline`s) and a started row for every member
  commit in one transaction. No body runs before that commit. The started row
  is the authorization; there is no permit or acknowledgement record.
- **Recovery.** A started `Once` member without an outcome records
  `Interrupted` and never polls its body. A started `Repeatable` member
  without an outcome runs again at the same ordinal, uncounted. A retry is a
  record with a due time; the next attempt takes the next ordinal.
- **Store-local effects commit with the tool result.** A built-in tool whose
  effect is a lash store write (process start, child session spawn, the store half of intent realization)
  performs that write in the transaction that records its outcome. Its effect
  is exactly once. A tool's plugin-state resolutions are such an effect: they
  ride its outcome record and publish into the resident namespace only from
  that committed record (FIG-5266). The namespace is the member's owner's:
  the session's for a round member or a code cell's call, the process's for
  an engine process's tool step, which a resumed activation rebuilds from
  its step outcome rows. A cell's quiet point that prunes a settled call's
  record first records the namespaces the run changed as the run's rows, so
  the pruned call's change is never replayed from its commands (FIG-5268,
  FIG-5301).
- **Group commit.** Finished members commit in batches, one transaction per
  batch.

### 6. Waits and timers are rows

- **Wait rows.** A wait is a row with a kind, an owner actor, an owner scope
  for revocation and an optional deadline written once at creation. The row exists from
  minting, so a resolution that arrives before the owner awaits finds it.
- **First winner.** Resolution is a conditional update from `pending`. A
  second resolution with the same digest answers `AlreadyResolved`; with a
  different digest, `Conflict`; on a revoked or timed-out wait, `Revoked`;
  with a key that names no wait, `Unknown`.
- **Completion keys.** A host-resolvable key is its wait's id: 128 random
  bits from the operating system's CSPRNG. It is a bearer capability, and
  lash keeps no completion secret: who may finish a pending wait is
  authorization, which the host owns (its API authentication, its webhook
  signatures), so it hands a key only to callers it has authorized
  (FIG-5217). The key carries no scope or kind. Hosts resolve only the `tool_completion` and `custom` kinds; a
  host resolution of any other kind answers `ReservedKind` and writes nothing.
  Turn cancellation and process terminals have their own admission paths.
  Host events settle deferring calls through completion keys under ADR 0136.
- **Timers are due times.** A durable sleep is a timer wait row, a backoff is
  a retry record, and every deadline is a column. A waiting actor carries the
  earliest due time; the ordinary claim picks it up and commits the timer
  resolution or `TimedOut { WaitDeadline }` before anything acts on it.
- **Suspension is a state.** An actor with nothing runnable commits its last
  phase, may stay hot until idle eviction, then releases as `waiting` and
  holds nothing. A running sibling body keeps the actor owned.

### 7. Execution policy and budgets

The policy layer of spec v3 Parts B, C and E carries over, minus every
journal-engine window.

- `ExecutionPolicy::{Once, Repeatable { retry: BoundedRetry }}` and
  `AttemptOutcome::{Completed, Waiting, Failed, Interrupted, TimedOut,
  Cancelled}` with `LimitCause::{ExecutionSlice, ExecutionTotal,
  WaitDeadline}`. `Once` never starts twice. `Repeatable` counts admitted
  ordinals. A current declaration may veto a repeat and never upgrades `Once`.
  `Interrupted` carries no fabricated partial.
- `ExecutionLimit` and `WaitDeadline` are recorded before their work starts,
  never refreshed, and nested limits take `min(own, enclosing remaining)`.
  An expired deadline found on load settles at once.
- `ExecutionBudgets` holds model, control-phase, stop-grace and provider
  attempt limits. Tool execution and park bounds are explicit host-provided
  manifest data under ADR 0136, with no Lash default or ceiling. Engine waits
  likewise require their own bound.
- Long work is a process tool, an isolated tool on a host engine, or a Pending
  tool. Body and park bounds are independent. A bounded park's deadline is
  fixed at admission; an `UntilScopeEnd` park is revoked by scope end.

- Recipients deduplicate external work on the lash-minted `ToolCallId`, which
  is stable across `Repeatable` ordinals under
  [ADR 0117](0117-lash-names-every-tool-call.md).

### 8. The VM snapshots at quiet points

Lash runs one heap VM; TypeScript lowers to it. A `VmContinuation` captures
all mutable execution state and resumes from it.

- The VM runs until it blocks on an await, or until its fuel slice ends. The
  host then commits, in one transaction, the next snapshot revision, the
  broker ledger that matches it, and the admission of every operation the VM
  issued since its last snapshot. Bodies start only after that commit.
- Broker ordinals are admitted operation identities, not journal positions.
  On restore the saved outcome of each admitted operation is fed back in; no
  earlier host operation re-runs.
- A clock or random read in an uncommitted stretch is drawn again after a
  crash. Nothing outside the VM observed it, because every effect that could
  carry it commits with the snapshot that contains it.
- A fuel yield that issued no effect may snapshot to bound recomputation.
  Recomputing an effect-free stretch is not replay.
- Issue-ordinal replay, the recorded frontier and the journaled binding set as
  a replay mechanism are deleted. Code cells resume from their snapshot.

### 9. Projections are providers

- **In the VM.** A projection value is plain data, `{ type, resource:
  ResourceRef }`. It snapshots with the heap and pins no node. VM state never
  holds a live host object.
- **On the host.** A `ProjectionProvider` is registered by type, as tools are
  in the catalog, with `read(resource, ProjectedReadRequest) ->
  ProjectedReadResponse` and a batched `read_range`. A read dispatches to the
  provider on whichever node runs the actor.
- **Reads are pure and `Repeatable`, and are not journaled.** A read before a
  snapshot is already in the heap; a read after it reads again. A provider
  that must answer identically after failover puts a revision or snapshot id
  in its `ResourceRef`.
- **`history`** is a lash-provided provider over the session transcript at a
  pinned revision.
- The live-object export registry (`Projections::export`,
  `exported_descriptors`), `HandOverRefusal::ExportedHostDescriptors`, the
  unavailable-after-restore placeholder and node pinning are deleted. Every VM
  run is snapshot-portable.

### 10. Host process engines are state machines

A host `ProcessEngine` is an explicit state machine:

```rust
fn advance(&self, state: EngineState, event: EngineEvent)
    -> Result<(EngineState, EngineAction), ProcessInfraError>;

enum EngineAction {
    Steps(Vec<StepRequest>),                         // admitted executions of catalog tools
    PinKey { name: KeyName, kind: HostWaitKind, bound: ParkBound },
    AwaitExternal { name: KeyName },                 // a key pinned earlier
    AwaitProcess { process: ProcessId, bound: ParkBound },
    Sleep { until: DurableInstant },
    Idle,                                            // until the next mailbox event
    Terminal(ProcessOutcome),
}
```

- A step names a catalog tool: its declaration's `ExecutionPolicy` and a host-set
  body bound are pinned at admission, and its body runs through
  the admitted-execution primitive (§5). There is no `perform`.
- The new state and the action's admission (its started rows or its wait row)
  commit in one `process.advance` transaction before the action runs. The engine state lives in the snapshot store under `p/<pid>`.
- `advance` performs no effects; effects happen only as actions. When the
  commit did not happen, the next pass calls `advance` with the same state and
  event, which is recomputation from committed state.
- Events come from rows, at most one per transaction: `Cancelled` (once),
  `Started`, the immediate answer to `PinKey` (`KeyPinned`),
  a step's `AttemptOutcome` under the §5 recovery rules, a resolved or
  timed-out wait, a process terminal, a sleep's end.
- There is no opaque `run`, no `await_terminal` and no re-invocation of host
  code from the top. No trait method has a default body.
- Cancellation is cooperative first. A running or waiting process receives
  `Cancelled { origin, grace_until }` once; `grace_until` runs from the
  committed request, and the steps admitted before it see their token. At
  `grace_until` lash commits a forced `Cancelled` terminal and drops the
  steps. Lash ends the process; it never claims a physical stop.

### 11. Cancellation cascades through the scope tree

- A cancel request is a mailbox row that wakes its actor, including a parked
  one.
- A parked process, one whose state this node cannot decode and one that never
  started end without running their engine: the claimer commits the terminal
  from registry state. A running or waiting process is cancelled through
  `advance` within its grace (§10).
- A process whose claims commit nothing parks with `ActivationLoop` at the
  activation-loop budget, recorded in the park feed; a cancel or an operator's
  redrive readies it.
- A process's terminal transaction resolves every `process_terminal` wait on
  it, revokes its own pending waits and begins the cascade over its
  `Until(process)` children. A turn commit and a session close do the same for
  their scopes through `end_scope`.
- The cascade is batched: each `cascade.batch` transaction cancels a bounded
  set of children and keeps a durable cursor for the rest; the ending actor
  stays runnable until the cursor is drained. No single statement walks a
  large tree. A parent's terminal does not mean its subtree is quiescent:
  `live_until_descendants` reads what is still live.
- A process-to-process wait is a `process_terminal` wait with an explicit host-set bound raced against
  the awaiter's own cancel mail, so a cycle of waits is cancellable.

### 12. The outbox keeps two kinds, and SQLite is one database file

Every cross-actor message is a mailbox write plus a wake in the producer's
transaction. Ingress, control intents, scope close, parent-end,
process start and process terminal stop being relayed obligations.
Two kinds remain:

- `SessionDelete`, as durable deferred work in the closing session actor;
- `ArtifactCleanup`, because it deletes bytes outside the database under
  [ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md).

The relay, claim tokens, re-arm and stall machinery of the other kinds are
deleted. On SQLite the process registry and durability core share the one
database file of the deployment, so every producer transaction spans every
table it writes.

### 13. The PostgreSQL durability assumption

`Once` holds across a database failover only if acknowledged commits survive
promotion. The host chooses its PostgreSQL topology. Lash documents this
assumption; it runs no startup check and offers no mode switch. Without it the
guarantee is "at most once, except on loss of acknowledged commits".

### 14. Testing doctrine

- Laws run the production runtime. There is no second scheduler and no engine
  double.
- A fault-injecting store wraps the transaction seam. Every commit carries a
  label. Cuts per label: fail before commit, commit with the acknowledgement
  hidden, stale epoch, zombie node and lost wake.
- A virtual clock is injected at the store clock, and `SimNodes` run several
  owners over one store.
- lash-sim's crash matrix enumerates commit labels.
- Storage laws run on SQLite file, SQLite memory and PostgreSQL.
- Tests are kept by purpose: domain laws stay, protocol and journal-index
  tests have no subject, and every deleted law has a written disposition in
  `docs/testing/substrate-port-ledger.toml`.

### 15. Kill criteria

Any one of these stops the substrate work for a redesign. None of them is
answered by adding a replay fallback.

- The vertical crash proof (one turn, one `Once` host operation, one VM
  snapshot, killed after the external work and resumed on another owner)
  cannot pass without hidden replay.
- The crash-matrix laws of §14 are not green by the end of the tool-round
  lane.
- VM snapshots need replay between snapshots ("snapshot every N blocks") to
  meet their cost.

## Scope

This decision replaces ADR 0104 and ADR 0103 and retires ADR 0111. Intent
realization's store half commits with its tool result under §5. The decision
rules of ADRs 0105, 0109 and 0110 apply on top of this decision: run admission's binding of rows and base head and the
Run's ownership of concurrent calls (ADR 0105), the two deferred-work kinds
`SessionDelete` and `ArtifactCleanup` (ADR 0109), and input ownership, abandon
writers and operator controls (ADR 0110).

There is no journal logic epoch, generation lane or build-generation
sentinel. Changing kernel code never requires a drain; changing a durable
format does, under [ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md).

## Rejected alternatives

- **Keep an external journaling engine behind an `EffectEngine` seam.** Two
  sources of durable truth stay, joined by a relay, and positional determinism keeps taxing every
  kernel change.
- **A store-backed engine that replays by key.** The shape of the deleted SQL
  replay driver. It rebuilds journal replay over SQL.
- **Re-entering a phase and serving its outcomes by name.** Keyed replay of
  kernel code. Phases are explicit commits instead.
- **Re-running a host engine's `run` from the top against keyed step rows.**
  Keyed replay of host code. Host engines are state machines (§10).
- **One opaque `run` per process.** It cannot suspend between its own waits
  without holding a node, and a crash loses all of its progress.
- **A lease-liveness check at commit beside the epoch.** It adds no safety,
  because only a claim or reap changes the epoch, and it fails commits of an
  owner nobody replaced.
- **Live host descriptors in VM state.** They pin a run to a node and make
  snapshots non-portable.

## Consequences

- Recovery reads state and is bounded by the size of that state. Lash owns its
  engine's bugs; the crash matrix over every commit label is the control.
- Zero-infra is one SQLite file with no server process.
- Host process engines and projection hosts change API: engines implement
  `advance`, and projections become registered providers.
- There are no determinism allowlists, journal budgets, segment cuts,
  forced-replay legs or engine server double.

## Design sources

- `/workspace/notes/lash/prospect-substrate/REPORT.md` (authoritative where the
  sources disagree)
- `/workspace/notes/lash/prospect-substrate/design-opus.md` sections 1 to 5
- `/workspace/notes/lash/prospect-substrate/verify.md`
- `/workspace/notes/lash/prospect-substrate/framing.md`
- `/workspace/notes/lash/prospect-longwork/spec-v3.md` Parts B, C and E

[ADR 0136](0136-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
