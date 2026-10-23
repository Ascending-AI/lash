# 0132: Durability is state-first over the lash store: actors, epoch fences, no replay

Recovery and external-effect guarantees follow [What survives a crash](../../README.md#what-survives-a-crash).

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

Lash owns no general OS-program execution engine. Its worker process and
transport are the execution boundary of ADR 0123; the bounded staging spool
of ADR 0145 §6 may spill pages to disk. Durable authority remains in the
configured store.

### 2. The no-replay rule

Recovery does not re-run code against a recorded history, by position or by
key, to rebuild committed state. It loads that state and continues from it;
computation after the last commit may run again.

- A committed phase record is state. Resume folds rows into state; the fold
  calls no producer.
- An uncommitted stretch of work is recomputed from the last committed state.
  It never consults an outcome recorded inside that stretch, because none is
  recorded: every effect whose outcome must survive commits as its own phase.
- Orchestration code (shift, turn loop, the Run coordinator, a host engine's
  logic) never re-executes to reach a recorded outcome.

The rule is a falsifiable law, checked by the crash matrix of §14. For every commit
label and every cut, after resume on another owner:

- NR-1. Every admitted identity with a committed outcome executes its body
  zero further times after that commit. A `Repeatable` body may have executed
  more than once before the outcome committed.
- NR-2. A `Once` identity started without an outcome executes zero further
  times and records `Interrupted`.
- NR-3. A `Repeatable` identity started without an outcome re-executes at its
  same ordinal, subject to the current policy. Recorded `Read` calls can also
  re-enter an interrupted ordinal. Unrecorded reads and computation after the
  last saved state may run again.
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

The acknowledged projection is an `OwnerState` under the actor, epoch and
state revision. Session state retains its frame projection, prompt/render and
lowering prefixes; process state retains its `Continuation`, engine state,
active run facts and resolutions. Warm passes fold only newly acknowledged
postimages and external frontiers. A dropped, failed or ambiguous commit
invalidates the projection before control escapes; a coherent cold load
replaces it. It cannot install a candidate after its claim ends. This keeps
warm reuse an optimization under the same durable authority.

`OwnerRetentionSettings` bounds resident sessions and processes together by
owner count and idle time. Busy owners count and cannot be evicted. Capacity
pressure evicts an eligible idle owner or defers activation; it never refuses
`send()` because owners are busy. Eviction releases the claim and all retained
references, and a later activation reloads committed state. Host ingress is a
separate byte bound checked before enqueue; it is not per-allocation accounting
or an RSS guarantee. The bounded commit byte budget must cover the ingress
bound, checked at `RuntimeHostConfig` construction.

The owner-retention and ingress ruling is generous, overridable defaults:
one hour idle, 10,000 resident owners, 64 MiB ingress, and a commit byte budget
at least as large as ingress. FIG-7198 implements this ruling; until it lands,
`DurableSettings` requires explicit owner-retention and ingress choices. The
ruling does not supply missing session configuration or tool execution bounds.

Implementation: `crates/lash-core-execution/src/runtime/actor/owner_state.rs`,
`crates/lash-core-execution/src/runtime/actor/process/continuation.rs`,
`crates/lash-core/src/runtime/durable/services.rs`,
`crates/lash-durable/src/runner.rs`, `crates/lash-durable/src/durable_config.rs`
and `crates/lash-core-execution/src/runtime/host.rs`.

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
  `model_total` deadline recorded before the first byte is sent. The
  provider's raw outcome commits on receipt as `model.answer`, with what the
  call's stream hooks saw, before any stream-finished or response hook runs;
  a restore after it derives the response again from the receipt and never
  re-sends the call. A crash before the receipt commits re-sends the pinned
  request as the next attempt while the deadline allows, even if the
  provider already answered and charged for it. An answer whose receipt
  owes no callback, in a turn no checkpoint or after-turn callback runs in,
  derives in memory before its receipt commits, and its receipt is held
  (FIG-6955). A final answer's receipt then commits with the turn's
  `turn.commit`. Any other answer's commits as `model.answer` before the
  next effect that is not an emission, the progress boundary or a
  callback-free checkpoint: before its tool round, its cell or the next
  call. A held receipt also commits on its own, as `model.answer`,
  whenever the turn leaves its phases without a `turn.commit` that carried
  it (FIG-6969): a failed derivation, an error in a local effect, a
  stalled machine, an `Immediate` cancel read at `turn.commit`'s open (the
  turn then stops from its last checkpoint on the next pass; an
  `AfterStep` cancel lets the turn commit, carrying the receipt), and a
  `turn.commit` that failed, whether or not its outcome is known. No error
  is classified as having written nothing. The store's fence decides
  instead: `TurnWrite::Answer` needs the run's phase row in its model
  phase, which a landed `turn.commit` pruned, so a receipt after a commit
  that landed (its acknowledgement lost, or a PostgreSQL `COMMIT` whose
  outcome the connection lost) is refused `TurnNotOpen` and its
  transaction writes nothing, and an owner that lost the session is
  refused by its epoch. A node killed or reaped before a held receipt
  commits leaves the call unanswered at `model.start`, as a crash before
  `model.answer` does, and the call is sent again. A cell may hold the
  derived response until its first save
  ([ADR 0142](0142-a-cells-first-save-carries-its-turns-model-response.md)).
  The call's sealed fact and `model_call_records` row commit beside a sealed
  receipt, in the `turn.commit` that carries a held receipt, or in the
  existing commit that settles an accepted cell's tail.
  The receipt carries `call_id`; consuming it retains the call record.
  Dispatch reservations retain every attempt ordinal before sending. A
  restoring owner records lost observations as `Interrupted`/`Unobserved`,
  with unknown usage. An accepted tail's pending identity survives receipt
  consumption until its settlement commits.
- Streaming is not durable. Deltas go to the live replay store; a re-sent call
  retracts what its earlier attempts published with one `ModelAttemptReset`
  read back from the live replay, then streams under ids of its own
  ([ADR 0040](0040-retried-model-attempts-retract-live-text-by-correlation.md)),
  so clients discard the abandoned stream and keep their cursors. Every
  block an attempt published is correlated under that attempt's stream
  key: its streamed deltas, and the whole blocks a derivation publishes
  for text the answer did not stream. A held receipt derives before
  anything durable holds it, so the reset retracts those whole blocks too
  (FIG-6969). The
  receipt records the visible chunks the stream hooks saw since the
  answering attempt began, so a derivation on any node replays them through
  the same hooks. The derived response commits with the next checkpoint and
  the round's admission in one transaction.
- The turn commit is one transaction: the session head compare-and-set,
  terminal evidence (also the turn-scope close fact), the parent-end of
  `Until(turn)` processes and retirement of the turn's working state. The
  head compare-and-set makes it idempotent. The turn's commit and its cancel
  retire the same state: the exec root, effect admissions and run records of
  every cell the turn stopped in, the turn's cancel request, and the turn's
  waits, compacted to tombstones (§6). The turn's own run records are its
  recovery ledger until then: the same commit publishes its transcript, the
  calls it answered by reference to the material its ledger named, and
  deletes the records (FIG-6601). Its report reads the transcript alone and
  no wait, so a report read after a crash and a redrive is the report the
  live turn gave (FIG-5939). Recovery of a live turn still reads its ledger
  and its full waits.
- A preparation hook recomputes from committed state when its phase did not
  commit, so hooks are repeat-safe. An at-most-once external mutation is an
  admitted `Once` execution, not a hook.

Events carry new information plus the identity of what was committed, never
a repackaged copy of committed state.
`TurnFinalized` carries only `session_id`, `turn_id` and the committed
`TurnCommitOutcome` kind. Hosts read that exact turn through
`DurableSession::committed_turn`; plugins use
`SessionReadService::committed_turn`. Both read the existing terminal,
transcript, the turn's call transcript and retained model-call records on any
node.
`read_model_calls` addresses each call by turn and call id, and
`committed_turns_with_calls` pages both kinds of call. Model-call billing belongs at
the provider seam (FIG-6547). `TurnHookReport` likewise exposes only the
pre-commit facts the callback actually has: the outcome and the tool-call
records, without invented usage, execution metrics or issues.

`TurnFinalized` lifecycle observers are advisory post-commit delivery. The
owner delivers them after the turn's commit is acknowledged and its live
commit observation is published or publication fails. The engine awaits
observers in registration order and reports their failures; those failures
cannot veto or change the committed outcome. A caller answers from the
stored terminal independently of this delivery, so its answer may precede
the observers. Delivery is not durable: an owner lost after commit may
never deliver the event. A consumer or law that needs an observation waits
for an explicit receipt from its observer, not for caller completion or an
elapsed time.

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
- **Tombstones.** When its turn ends, a wait the turn scoped is compacted to a
  tombstone: its id, kind, final state, resolution digest and resolution time,
  with its owner scope kept for the session's delete. The tombstone is marked
  by a checked column, and a full read of a wait refuses it. A late or
  repeated resolution is answered from the tombstone as above. The rows of a
  deleted session or a pruned process are deleted with their owner, and a key
  of theirs then answers `Unknown` (FIG-5939).
- **Completion keys.** A host-resolvable key is its wait's id: 128 random
  bits from the operating system's CSPRNG. It is a bearer capability, and
  lash keeps no completion secret: who may finish a pending wait is
  authorization, which the host owns (its API authentication, its webhook
  signatures), so it hands a key only to callers it has authorized
  (FIG-5217). The key carries no scope or kind. Hosts resolve only the `tool_completion` and `engine_key` kinds; a
  host resolution of any other kind answers `ReservedKind` and writes nothing.
  Turn cancellation and process terminals have their own admission paths.
  Host events settle deferring calls through completion keys under ADR 0137.
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

- `ExecutionPolicy::{Once, Repeatable { retry: BoundedRetry }, Read}` and
  `AttemptOutcome::{Completed, Waiting, Failed, Interrupted, TimedOut,
  Cancelled}` with `LimitCause::{ExecutionSlice, ExecutionTotal,
  WaitDeadline}`. `Once` never starts twice. `Repeatable` counts admitted
  ordinals. A current declaration may veto a repeat and never upgrades `Once`.
  `Interrupted` carries no fabricated partial.
  `Read` records nothing about a call a code cell plans as a read
  ([ADR 0144](0144-declared-reads-are-owned-by-the-callers-saved-state.md)).
- `ExecutionLimit` and `WaitDeadline` are recorded before their work starts,
  never refreshed, and nested limits take `min(own, enclosing remaining)`.
  An expired deadline found on load settles at once.
- `ExecutionBudgets` holds model, control-phase, stop-grace and provider
  attempt limits. Tool execution and park bounds are explicit host-provided
  manifest data under ADR 0137, with no Lash default or ceiling. Engine waits
  likewise require their own bound.
- Long work is a process tool, an isolated tool on a host engine, or a Pending
  tool. Body and park bounds are independent. A bounded park's deadline is
  fixed at admission; an `UntilScopeEnd` park is revoked by scope end.

- Recipients deduplicate external work on the lash-minted `ToolCallId`, which
  is stable across `Repeatable` ordinals under
  [ADR 0117](0117-lash-names-every-tool-call.md).

### 8. The VM snapshots at quiet points

Lash runs one kernel machine; every dialect lowers to it. Its execution
state is one persistent heap, and a snapshot is a checkpoint root over that
heap's pages
([ADR 0145](0145-kernel-execution-and-its-checkpoints-share-one-persistent-heap.md)).

- The VM runs until it blocks on an await, or until its fuel slice ends. The
  worker then seals the changed pages into a candidate root, and the host
  commits, in one transaction, that root as the next snapshot revision, the
  broker ledger that matches it, and the admission of every operation the VM
  issued since its last snapshot. Bodies start only after that commit.
- Broker ordinals are admitted operation identities, not journal positions.
  On restore the saved outcome of each admitted operation is fed back in; no
  operation with a committed outcome re-runs. Started operations without
  outcomes follow their execution policy.
- A clock or random read in an uncommitted stretch is drawn again after a
  crash. Recorded effects carrying the value are admitted with the snapshot that
  contains it. Printing, logging and advisory observations may already have
  exposed it; those diagnostics may repeat or be lost.
- A fuel yield that issued no effect may snapshot to bound recomputation.
  Recomputing an effect-free stretch is not replay.
- Issue-ordinal replay, the recorded frontier and the journaled binding set as
  a replay mechanism are deleted. Code cells resume from their snapshot.

### 9. Transcript and host data are Read tools

A host or protocol contributes ordinary package operations whose tools declare
`ExecutionPolicy::Read` and whose provider registers `ToolProviderRegistration::with_reads`.
A cell's read-bound `perform` is answered inside its statement, without a park,
journal record or per-read audit (ADR 0144). Its answer survives in the next
saved state. Host data is never a special VM binding or projection handle.

The RLM protocol owns `rlm.history.length()`, `rlm.history.at(i)` and
`rlm.history.slice({from, to})`. The runtime passes the immutable history view
of the cell's run, restored from its turn's checkpoint window and content
prefixes. The turn checkpoint retains its immutable window identity and its content
prefixes. Compaction and frame switches cannot substitute a newer window.
If retention has released the admitted base, restore refuses with the existing
typed `StoreError::TurnBaseNotRetained`, carried through runtime recovery as
`StoreRefusal::TurnBaseNotRetained { revision }`, before any cell read runs. Reads project only
returned items. Negative indices count from the end; out-of-range `at` returns
null (Python `None`). Slice bounds clamp, its end is exclusive, and requests
above 256 entries refuse with `history_slice_limit`: callers page explicitly.

Hosts expose data through their own Read tools. The projection-provider API,
projection-read kernel expression and eager read-only projected bindings are
deleted. Continue-as seed entries are ordinary writable session variables.

### 10. Host process engines are state machines

A host `ProcessEngine` is an explicit state machine:

```rust
fn advance(&self, state: EngineState, event: EngineEvent)
    -> Result<(EngineState, EngineAction), ProcessInfraError>;

enum EngineAction {
    Steps(Vec<StepRequest>),                         // admitted executions of catalog tools
    PinKey { name: KeyName, bound: ParkBound },
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
- An engine's own step bodies (`EngineSteps`) run the same way under the
  retry policy their kind declares. A body the host lacked capacity for (a
  full worker queue, a timed-out checkout, a restart storm, or retryable
  worker infrastructure) answers a typed `CapacityVerdict` instead of an
  outcome (FIG-6155). The lifecycle honours it only from a `Repeatable` step
  that the current declaration still repeats. Such a step's body performed
  nothing outside it, or recomputes from committed state. Honoured, the
  attempt records nothing and spends none of the step's attempts. The
  activation backs off by the verdict's `retry_after`, doubled for each
  claim in a row without progress, and ends without committing. The next
  claim counts a failed activation (§11) and reruns the step at its same
  ordinal. Persistent saturation therefore parks the process with
  `ActivationLoop`, which is redrivable, and never ends it. From any other
  step the verdict is refused, and the attempt records `Interrupted`.
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
  activation-loop budget, recorded on the actor row; a cancel or an operator's
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

### 13. The storage durability assumption

Committed-outcome recovery and NR-2's refusal to restart a `Once`
body survive power loss only when every boundary commit is durable before
it returns. SQLite file stores require `SqliteSynchronous::Full`, with storage
that honors sync requests. `Normal` may lose acknowledged commits; `Off` may
also corrupt the database. Both are explicit weaker contracts: losing a
recorded start after its body ran can admit that body again. The host still
states the SQLite mode explicitly; there is no default (D-DEFAULTS2).

The SQLite writer group-commits (FIG-5958): the writes already queued when it
becomes free share one `BEGIN IMMEDIATE`, one writer fence and one durable
`COMMIT`, each in its own savepoint, so one member's refusal undoes only that
member. No member answers before the shared commit returns, and a failed commit
fails every member. Nothing waits for a batch to fill, and a lone write runs as
a plain transaction. The synchronous mode stays the connection's, fixed before
any transaction begins.

A grouped member gets exactly the answer its solo commit would have got, and
it never changes a sibling's answer (FIG-5970). When SQLite makes that
impossible because the whole transaction ended, every affected member fails
with a typed, retryable cause and none is reported committed. SQLite checks a
deferred foreign key (`session_head` to `session_revisions`) only at the
outermost `COMMIT`, so after each kept member with siblings the writer reads
`SQLITE_DBSTATUS_DEFERRED_FKS`. A member that left one violated is undone to
its savepoint and answered the `SQLITE_CONSTRAINT_FOREIGNKEY` failure its solo
`COMMIT` would answer: it can neither fail its siblings' `COMMIT` nor be healed
into a commit by a later sibling. A panic in a member's body is that member's
failure: its savepoint, or a lone write's transaction, is undone and it
answers a storage failure naming the panic, while its siblings run on and the
connection thread keeps serving.

SQLite may end the whole transaction on an error inside a statement
(`SQLITE_FULL`, an I/O error, `SQLITE_NOMEM`), not only the member's savepoint,
so the writer checks for autocommit after every member (FIG-5968). A member's
failure then affects another member only where SQLite took that member's work
with it. A member keeps its own answer when it kept no writes and no member
before it kept any, since its answer was decided against committed state alone;
the member whose own error ended the transaction keeps that error. Every other
member that ran fails with a storage failure naming the ended transaction and
is never answered committed. Members not yet run lead the next batch untouched.
Member bodies run once, so a lost member is failed, not re-run.

PostgreSQL requires `fsync=on` and synchronous commit (`synchronous_commit=on`
or a synchronous level that waits for local WAL flush). Asynchronous commit
can lose acknowledged starts; disabling fsync also risks corruption. Every
boundary commit retains the configured durability; there is no asynchronous
interior tier or later barrier that substitutes for its commit.

Performance gates and E2E harnesses measure this promised configuration.
Hermetic PostgreSQL laws and CI may disable flushes to test engine logic;
they do not certify power-loss durability. Process-kill laws alone do not
simulate power loss.

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
- Host process engines implement `advance`; host data readers register Read tools.
- There are no determinism allowlists, journal budgets, segment cuts,
  forced-replay legs or engine server double.

## Design sources

- `/workspace/notes/lash/prospect-substrate/REPORT.md` (authoritative where the
  sources disagree)
- `/workspace/notes/lash/prospect-substrate/design-opus.md` sections 1 to 5
- `/workspace/notes/lash/prospect-substrate/verify.md`
- `/workspace/notes/lash/prospect-substrate/framing.md`
- `/workspace/notes/lash/prospect-longwork/spec-v3.md` Parts B, C and E

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
