# 0105: The drive is deterministic workflow code

## Status

Accepted 2026-09-24 (FIG-3672, slice P0). The open questions of the FIG-3672
plan were ruled the same day; they are recorded as decisions under
*Rulings*. **Not yet implemented**: the traits and types of the contract exist
in `lash-core-execution::engine` (re-exported as `lash_core::engine`), with no
engine implementing them and no drive consuming them. The FIG-3672 slices P1
to P18 build the rest, in the order under §13. Nothing below describes current
behaviour unless it says so.

Refines the execution seam of
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
§2: that ADR requires the seam, and this one states it. It is the prerequisite
of FIG-3600 S5, which freezes the drive API against it.

The evidence is the seam proof
(`/workspace/notes/lash/prospect-unreal-agent-lanes/lane-seam-proof.report.md`),
its contract review (`/workspace/notes/lash/fig3664-engine-contract/astra-seam-report.md`)
and the FIG-3672 inventory and slice plan
(`/workspace/notes/lash/prospect-unreal-agent-lanes/lane-3672-plan.report.md`,
baseline origin/main `dd80a5e7c`).

## Context

The turn machine and the effect commands are already replayable: the
machine's transitions are synchronous, and commands and outcomes are serde
values with explicit replay identities. The driver around them is not
deterministic. The inventory counts 238 decision-path sites in eight
categories: `select!` races against live cancel tokens, spawned forwarders and
cell tasks, channel arrival order, wall-clock reads and CPU-time budgets,
store I/O in drive code, lazy caches and mutable side channels, and async
hooks that decide outcomes without being recorded. Five findings reach past
the seam proof:

- the committed turn is assembled from the observation stream;
- five execution-side bodies leak state into the drive outside their
  recorded outcome;
- `PresentToolResult` hashes a wall-clock duration into its envelope;
- the Restate group-child dispatch and the process-segment workflow have
  their own unjournaled cancel races;
- the main Restate effect journal has no version gate.

This matters for Restate replay today, not only for a future Temporal adapter.
A replay that re-derives a decision from a clock, a cache or a race it did not
record can issue different effects from the ones its journal holds.

A drive is deterministic when every decision input is either a recorded
outcome, a recorded immutable input, or a pure function of those. The only
place a drive may wait is an operation the engine records.

## Decision

### 1. The drive awaits only engine-context operations

An engine implements `EngineContext` once. Every operation returns the
engine's own op type:

```rust
pub trait EngineContext {
    type Op<'a, T: 'a>: DurableOp<T> + 'a where Self: 'a;
    fn now_ms(&self) -> Self::Op<'_, EpochMs>;
    fn version(&self, change: &'static ChangeId, supported: VersionRange) -> Self::Op<'_, u32>;
    fn observe(&self, observation: DriveObservation);
    fn race<'r, 'a: 'r, 'b: 'r, A: 'a, B: 'b>(
        &'r self,
        first: Pin<&'r mut Self::Op<'a, A>>,
        second: Pin<&'r mut Self::Op<'b, B>>,
    ) -> Self::Op<'r, Winner<A, B>> where Self: 'a + 'b;
    fn dispose<'a, T: 'a>(&self, op: Self::Op<'a, T>, how: Disposition)
        -> Self::Op<'_, Disposed> where Self: 'a;
}
pub trait DurableOp<T>: Future<Output = Result<T, EngineFault>> + FusedFuture + Unpin {}
pub enum EngineFault { Suspended, Retryable(EngineRetry), Terminal(EngineTerminal) }
```

- **Only context futures.** Drive code awaits nothing else: no Tokio channel,
  `Notify`, `select!`, spawned task, live `CancellationToken`, store call or
  lock. Law L-X1 (§11) enforces it.
- **Ops are fused and `Unpin`.** A race may poll a finished arm again, and a
  raced op can still be handed to `dispose` by value. An engine whose native
  futures are not `Unpin` boxes its concrete future type; that is a box, not a
  `dyn Future`.
- **Engine faults are not domain outcomes.** The engine retries `Retryable`
  under its own policy and never records a fault as a result. Domain failures
  ride `EffectResult`.
- **Time is recorded.** `now_ms` is recorded on first execution and replayed.
  Every deadline in a command is an absolute `EpochMs` taken from it; no
  `Instant` crosses the command surface.
- **Observation never decides.** `observe` is synchronous, never wakes the
  drive, is keyed by replay key and ordinal, and is suppressed or deduplicated
  on replay. Anything a commit or a later decision reads comes from recorded
  outcomes, never from the observation stream.

### 2. Unfenced admission, separate from fenced execution

Only three operations exist before a fence exists:

```rust
pub trait DriveAdmission: EngineContext {
    fn admit(&self, req: &AdmitRequest) -> Self::Op<'_, AdmitVerdict>;
    fn seal(&self, admitted: Admitted) -> Self::Op<'_, SealVerdict>;
    fn inherit(&self, authority: &InheritedAuthority) -> Self::Op<'_, InheritVerdict>;
}
```

- **`admit`** reads the session head generation (the FIG-3619 gate), the
  drive epoch, the parked-root set and the root's start marker. It answers
  `Admit`, `Parked`, `SubstrateLost`, `RootTerminal` or `Idle`.
- **`seal`** advances the drive epoch by one with a compare-and-set, sets the
  root's start marker if absent, keyed by the admission nonce, and revalidates
  authority in the same transaction. A reset before the seal may retain
  `admit`'s result without rerunning it, so the seal cannot trust it.
- **`inherit`** is a child's check of the authority it inherited. A group
  child or child execution **never mints an epoch**; it answers `Valid`,
  `Stale` or `GenerationRefused`.

Every other durable operation is reachable only through `Fenced`, which is
built only from a recorded `SealVerdict::Sealed` or `InheritVerdict::Valid`:

```rust
pub struct Fenced<'c, C: DriveContext> { /* private: the context and the fence */ }
impl<'c, C: DriveContext> Fenced<'c, C> {
    pub fn step(&self, cmd: EffectCommand) -> C::Op<'c, EffectResult>;
    pub fn timer(&self, at: EpochMs, key: &ReplayKey) -> C::Op<'c, ()>;
    pub fn await_key(&self, key: &AwaitEventKey) -> C::Op<'c, Resolution>;
    pub fn peek_key(&self, key: &AwaitEventKey) -> C::Op<'c, Option<Resolution>>;
    pub fn resolve_key(&self, key: &AwaitEventKey, r: Resolution) -> C::Op<'c, ResolveAck>;
    pub fn turn_cancel(&self, scope: &ExecutionScope, gate: CancelGate) -> C::Op<'c, TurnCancelSignal>;
    pub fn child(&self, start: ChildStart) -> C::Op<'c, ChildOutcome>;
    pub fn fence(&self) -> &DriveFence;
}
```

`DriveFence` has no public constructor. It is decoded only from a recorded
verdict and is never part of an envelope hash (L-S12). An effect without a
fence is unrepresentable.

### 3. A race keeps its loser

`race` reborrows both arms. It completes with the first arm the engine records
as completed:

- **Fresh execution:** when both arms are ready at the same engine poll, the
  first declared arm wins.
- **Replay:** the recorded winner is returned.
- **The loser is untouched.** It stays owned by the caller, pending and
  durable. The caller may await it, race it again, or dispose of it.

`dispose` is the only way to give up an op, and dropping an op without
disposing of it means `Abandon`:

| Disposition | Step | Key wait | Child |
|---|---|---|---|
| `Abandon` | keeps running; its result is ignored and stays recorded | unregistered | the parent-close policy abandons it |
| `RequestCancel` | engine cooperative cancel; the external operation is idempotent under its operation id | resolved `Cancelled`, first writer wins | cancel requested |
| `AwaitCancelled` | as `RequestCancel`, then waits for the recorded terminal | as `RequestCancel` | as `RequestCancel`, then waits |

This preserves today's `AfterStep` then `Immediate` escalation:

```rust
let mut step = f.step(cmd);
let mut gate = f.turn_cancel(&scope, CancelGate::Turn);
match cx.race(Pin::new(&mut step), Pin::new(&mut gate)).await? {
    Winner::First(out) => { cx.dispose(gate, Disposition::Abandon).await?; out }
    Winner::Second(TurnCancelSignal::Immediate(ev)) => {
        cx.dispose(step, Disposition::RequestCancel).await?; cancelled(ev)
    }
    Winner::Second(TurnCancelSignal::AfterStep(ev)) => {
        let mut esc = f.turn_cancel(&scope, CancelGate::Escalation);
        match cx.race(Pin::new(&mut step), Pin::new(&mut esc)).await? {
            Winner::First(out) => { cx.dispose(esc, Disposition::Abandon).await?; stop_at_boundary(out, ev) }
            Winner::Second(_) => { cx.dispose(step, Disposition::RequestCancel).await?; cancelled(ev) }
        }
    }
    Winner::Second(TurnCancelSignal::SessionRevoked) => {
        cx.dispose(step, Disposition::RequestCancel).await?; revoked()
    }
}
```

The engine tests in `lash-core-execution` run this shape on a `!Send` and a
`Send` test engine.

### 4. Group operations are complete

`DriveGroups` lifts today's controller surface onto the context and makes it
fence-aware: `open_group` (records membership, reopen fenced on shape),
`next_settlement` (the rank cursor), `read_settlement` (cursorless),
`settled_count`, `commit_child_final` (the cancel fence), `await_drain_admission`
(the protected-drain barrier) and `close_group`.

- **Rank contract.** A settlement's rank is assigned when the engine records
  it, at the group's single serialization point. Ranks are dense per group.
  On `close(Cancel)`, undecided children are seated in ascending position
  order. A replay observes the same child at rank n.
- **The cursor.** The handle is the only cursor of record. `next_settlement`
  serves the rank after `consumed()`. When it loses a race against a cancel
  gate, the cursor is untouched.
- **Close narrows only.** Under `Cancel` it seals cancel decisions before any
  interrupt, and excludes children that are committed but unseated: they
  drain and then seat their own rank. Close is idempotent.
- **Opener bookkeeping leaves the context.** Reserve and bound, the held list
  and the incorporation ledger become a pure fold over recorded outcomes.

### 5. Keyed promises

- **Resolve before wait.** A resolution persists whether or not a waiter
  exists, and `await_key` returns it.
- **Dedupe.** The first writer wins. `ResolveAck` is `First`,
  `Duplicate { same }` or `Revoked`, and a differing duplicate never
  overwrites.
- **Revocation.** Revoking a session resolves every waiter `Revoked`, and a
  later resolve is acknowledged `Revoked`.
- **Acknowledgement.** An ack is returned only once the resolution is durable.

An engine whose signals are dropped when no handler is registered (Temporal)
implements keyed promises as signal-with-start wait executions per key digest,
never as bare signals.

### 6. Protocol drivers and hooks are pure

- **Protocol drivers.** `ProtocolDriverHandle` and `ContextProjector` methods
  are synchronous, take `&self` and have no side effects. A replay calls them
  again over the same recorded inputs and must reach the same decision.
  Interior mutability in an implementor is a contract violation, and the P5
  lint checks the protocol crates for it.
- **Plugin hooks are pure and synchronous over recorded inputs.** Their I/O
  runs in declared steps.
- **Hooks get read-only services.** A hook reads recorded state through
  read-only views. Every write, including a graph append, a frame switch, a
  session create and a tool-state apply, is a recorded command whose outcome
  is folded. Standard compaction's frame switch is the recorded outcome of its
  checkpoint step.
- **A group child's graph append is its own recorded command.** Its outcome
  rides the child's settlement, and the opener incorporates it at its rank. A
  child that settles after the opener has released has its append refused
  typed, never silently dropped.

### 7. Continue-as-new and version decisions

**Handover.** A new engine run does not inherit the previous run's history. A
session execution hands over only at a turn boundary, when the deterministic
`handover_suggested()` says so, and only after every signal handler has
finished. `continue_as_new(DriveHandover)` never returns. The handover
carries:

- the fence (the epoch is not re-minted);
- resolved but unconsumed keys;
- a window of already-admitted drive request ids;
- open groups with their rank cursors;
- unresolved children with their inherited authority;
- the version decisions still in force;
- the active root's progress.

**An oversized single turn** hands over at its next checkpoint boundary, as a
`TurnSegmentHandover` carrying the turn machine's checkpoint and the opener
fold, both by blob reference. If it cannot, it parks as journal-budget
exhausted ([ADR 0025](0025-bounded-journals-are-an-effect-controller-obligation.md)).
The contract is frozen now; an engine implements it when it needs it.

**Version decisions.** Both mechanisms are frozen:

- `version(change, supported)` is recorded. A fresh execution takes
  `supported.max()`, and a replay returns the recorded value. It is for
  in-place code patches.
- `DriveRequest.build_generation` pins the deployment, so the engine routes a
  replay to a compatible build.

Durable format changes keep using the generation gates (§12).

### 8. Send and !Send

The contract uses static dispatch: every future on the workflow-side chain is
an engine's `Op` type, and no `dyn Future` appears on it. Auto traits leak
through monomorphization, so a drive over an engine with `Send` ops is `Send`,
and one over an engine with `!Send` ops (Temporal's workflow context is `Rc`)
is `!Send`, with no bound of its own. Nothing bridges back through Tokio
channels. `Send` sits on the executor registries (§10), which run on the
execution side.

Two compile tests prove it: a `!Send` instantiation, and a `Send` one asserted
with `assert_send`. P0 proves them for the contract with test engines; P17
proves them for the whole drive chain. Twin traits, macro-duplicated chains and
a `LocalSet` bridged by channels are rejected.

### 9. Commit, park and settlement are recorded steps

A turn commit is one immutable, serializable `TurnCommitRequest`. It holds no
`Arc`, `Mutex`, clock, session state or graph editor. A pure function of the
machine state and the recorded outcomes builds it. The commit id derives from
the logical root and the physical ordinal, never from an epoch or a clock.

- **Transactional fencing.** The `CommitTurn` body checks the fence epoch
  against the head's drive epoch, the commit id and the root's terminality in
  the same store transaction as the head compare-and-set. The same holds for
  `RecordPark` and `SettleIngress`.
- **Outcomes.** `Committed`, `AlreadyCommitted` (a retried body found its own
  commit: same id, same bytes), `RootAlreadyTerminal` (another commit closed
  the root; it is preserved, never replaced), `StaleFence` and `HeadConflict`.
- **External operations are separate.** An epoch check cannot retract a
  request already sent. A model call or tool call carries
  `StepContext.operation` as its idempotency identity (ADR 0104 O1).
- **An external recovery writer remains**, `ParkRecoveryWriter`, for when
  workflow-task nondeterminism stops the workflow from issuing `RecordPark`.
  Its idempotency key is the root and the park id, the same as `RecordPark`'s,
  so the two writers converge. Park reconciliation is its only caller.

The component types of the request are P0's proposal. P15, which first issues
`CommitTurn`, owns their final shape; none is a registered durable format yet.

### 10. Commands and executors

`EffectCommand` is today's `RuntimeEffectEnvelope` and `EffectResult` is
`Result<RuntimeEffectOutcome, RuntimeEffectControllerError>`. Two executor
registries, `AdmissionExecutors` and `EffectExecutors`, resolve a body from
serialized input alone and run it inside the engine's recorded body. Neither
ever runs in drive code. A step body gets a `StepContext`: the fence, the
operation id, the attempt, a heartbeat, an observation sink, a cooperative
cancel and the session services, rehydrated from the session id on any
worker. `SessionServices` has no members yet; P10a defines them.

- **Canonical-envelope validation on replay.** Every step result is recorded
  with its canonical envelope, and the adapter compares it before returning a
  replayed result. An engine whose replay matcher compares only ids and types
  (Temporal's activity matcher) returns `{envelope_hash, outcome}` as the
  activity result and compares it in workflow code. A mismatch is
  `EffectReplayDivergence`, which parks.
- **Memory projection refs are refused on durable paths.** A `memory` ref is
  worker-local by definition, so a replay on another worker cannot resolve
  it. Every durable path (envelope, global, seed) refuses it typed; embedders
  register durable, content-derived kinds. This breaks the facade's
  `ProjectionRegistry::register_memory` (P4).
- **Command additions belong to their slices.** `RuntimeEffectCommand` gains
  `AdmitDrive`, `SealDriveAdmission` and `ValidateInherited` as admission
  commands (S5); `CommitTurn`, `RecordPark`, `SettleIngress`,
  `CaptureExecutionState`, `CapturePluginStates`, `ResolveTurnTerminal` and
  `RecordParentEnd` (P15); `GroupSettledCount` (P14); `PrepareToolCalls`,
  `PublishExecutionEnv`, `PutAttachment`, `PublishModuleArtifact` and
  `ResolveProcess` (P12, P13); and `AppendSessionNodes` (P11).
  `RuntimeEffectOutcome::{LlmCall, ExecCode, Checkpoint, SyncExecutionEnvironment}`
  gain the decision half of today's turn-effect state update (P7). None uses
  `#[serde(default)]`: the P1 gate refuses old bytes.
- **Implemented (P7, first part).** The turn-effect state update is deleted: a
  step body runs on a copy of the driver and hands back nothing but its
  outcome.
  - The checkpoint's claims reach the driver only through its recorded claim
    set, folded in before its result is read, so a failed checkpoint hands
    its claims to the failure path on replay as on the live pass.
  - A model call runs on a copy of the provider the turn was admitted with;
    nothing the provider object learns during a call reaches the next one.
    The `LlmCall` command names the policy's provider beside the request's
    model.
  - The `LlmCall` outcome carries what the provider stream left for later
    steps: the reasoning blocks it already published, and each plugin's
    stream-hook end state. The `AssistantResponseHooks` command carries those
    states, and a response hook reads them, never its plugin's memory, so
    phase 2 derives the same response on any worker. The stream-finished hook
    returns the state.
  - Trace ids for model calls are named by their effect, not by a counter
    carried between steps.
  - Effect-journal generation 2.
  - Held for a ruling: the tool surface (the sync body's catalog cache, the
    drive-side machine preparation, catalog drift checks and plugin tool
    overlays).

### 11. Validation and laws

Every slice owes three tests where they apply:

- **cold replay:** drop and replay, with always-replay mode where available;
- **separate worker:** a fresh runtime with no live openers, caches or warm
  RLM state replays the recorded journal;
- **perturbed scheduling:** a single-threaded executor with seeded yields and
  poll-order shuffles between ready ops, comparing the command stream and the
  canonical bytes of the commit request across seeds.

They run on the in-tree Endpoint double until FIG-3665 lands, then on the
`lash-restate-test` runtime through its one constructor,
`lash_restate_test::backend(seed, cfg)`, shaped for the B2 construction
(`Backend::new(engine, stores)`, ADR 0104 §2).

The seam proof's per-session laws stand, amended:

- **L-S3 and L-S4 assert one durable seal:** exactly one drive-epoch
  transition in the store's epoch history, whatever the number of
  seal-body invocations. Lost replies cause retries, so counting body
  invocations proves nothing.
- **L-S6: a later epoch adopts an already-committed root.** After a lost
  commit reply and a takeover, the committed root is preserved, never replaced
  with newer content. `admit` reads the root-addressed terminal evidence
  (ADR 0101, FIG-3607 contract 2) and answers `RootTerminal`.
- **L-X1 is widened.** Beyond failing on any wake not caused by a context op,
  it compares replays under perturbed scheduling, because a wake tracker alone
  does not catch clock reads, RNG, mutable globals or immediately ready I/O.

### 12. The Restate journal generation cutover

- **Implemented (P1).** Every entry a recorded effect journals in its
  `lash:{replay_key}` slot carries `effect_journal_version`, stamped with
  `EFFECT_JOURNAL_VERSION` (`lash-restate`, registered in
  `scripts/versioned-surfaces.toml` and `lash::formats` as
  `RestateEffectJournal`). An entry stamped with another generation, or with
  none, still decodes, so the SDK never loops on a decode failure, and the
  controller refuses it as the engine-neutral `effect_replay_divergence`
  before the replay acts on it. The refusal parks the turn: the attempt fails
  retryably, the invocation keeps its journal, and only a build of the
  generation that wrote it replays it.
- The gate sits at each recorded effect's entry. A durable process command
  runs its work before its entry is journaled, but in a turn it always follows
  the recorded model call that asked for it, and a process segment's journal
  is refused first at admission by its own `RESTATE_PROCESS_JOURNAL_VERSION`.
  So an old journal is refused before a replay runs any effect.
- **Every later slice that changes the bytes a recorded effect journals, or
  the position of a recorded effect in the journal, bumps
  `EFFECT_JOURNAL_VERSION`.** The surface's guards fail CI on an unbumped
  shape change to the entry, the envelope and outcome vocabulary, or the
  controller error an outcome carries.
- S5 adds the input stamp for the session and turn handlers and the
  session-state generation bump that covers new admission entries.
- The process journal keeps its own version, which P16 bumps.
- The admission marker's nonce seal and its missing-history refusal are
  preserved as two separate steps, and S5 copies them for drives.

Old histories are refused typed and fail closed; there is no migration or
drain.

### 13. Order and ownership

P0 (this ADR and the contract types) comes first. P1 to P7 can start now; P8
to P10, P14 and P16 wait for FIG-3585 so they do not convert code it deletes;
P14 lands after the group-child fixes; P15 lands after FIG-3659 NOW-A; P18
needs FIG-3665. The critical path is FIG-3585, P9, P10a, P10c, P17, P18.
**P16, the process drive, belongs to the lane that owns `lash-restate`'s
process workflow**, which builds against this contract and lands after P9.
FIG-3672 does not convert lease code that FIG-3667 and FIG-3668 delete.

## Rulings

The plan's open questions, as ruled on 2026-09-24:

| Question | Ruling |
|---|---|
| QB1 Race API | Race reborrows both arms; `dispose` is explicit; a dropped op means abandon (§3). |
| QB2 An oversized turn under continue-as-new | The handover contract is frozen now; an oversized turn hands over at its next checkpoint boundary or parks (§7). |
| QB3 Memory projection refs | Refused on every durable path. The facade break is allowed (§10). |
| QB4 Hooks that receive services | Hooks get read-only services; writes are recorded commands (§6). |
| QB5 Group-child graph appends | A recorded command of the child, incorporated at its rank (§6). |
| QB6 Who owns P16 | The lane that owns the Restate process workflow (§13). |
| QB7 Send and !Send | Static dispatch, no `dyn Future` on the workflow side, two compile tests (§8). |
| QB8 Versions and deployment pinning | Both: a recorded `version()` op and `build_generation` pinning (§7). |
| QB9 L-S6 | A later epoch adopts an already-committed root (§11). |

## What is deliberately not adopted

- **A pure poll interface for the whole drive.** The turn machine already is
  one. The lashlang VM awaits effects inline, and a poll seam would force a
  suspendable VM at every aggregate. Async code over recorded ops is
  replayable once it awaits nothing else.
- **A consuming race that returns the loser.** It forces the loser to be
  re-wrapped, which breaks the escalation in §3.
- **Twin `Send` and `!Send` traits, or a chain duplicated by macro.** They
  double the surface. Running `!Send` under a `LocalSet` bridged by channels
  is the pattern this ADR removes.
- **Forbidding oversized turns.** It is not viable for long agent runs.
- **Fencing external effects by epoch check.** A check cannot retract a
  request already sent; idempotent operation ids cover that.

## Consequences

- **Replay on Restate becomes correct by construction** once the slices
  land: a replay cannot re-derive a decision from anything it did not record.
- **A second engine is a crate.** It implements `DriveGroups` and the executor
  registries, and passes the laws.
- **Every byte-changing slice is a journal cutover.** Histories written before
  it are refused typed, never migrated.
- **Hook authors lose write access.** Hooks that wrote the store or the graph
  directly declare commands instead.
- **Cost: boxing.** An engine whose native futures are not `Unpin` boxes its
  op type, one allocation per op.
- **Risk: the contract's first implementation finds a gap.** The traits have
  no implementor yet. The P0 test engines exercise the race, the escalation
  and both instantiations; the Restate implementation arrives with the drive
  slices, and any change to this contract amends this ADR.
