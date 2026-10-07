# Substrate seams

I0 (FIG-5194) pins the interfaces the substrate lanes share (ADR 0132) as
compiled skeletons. A seam's real parts are I0's; each part another lane
fills is a `todo!("<lane> (<ticket>): <what>")`. Every such stub is listed in
the [stub inventory](#stub-inventory) below. `scripts/check-substrate-todos.py`
fails on a stub without a known lane tag, and its `--final` mode (run by L13,
FIG-5193) fails on any stub that remains.

Lanes: V0 = FIG-5170, L3 = FIG-5172, L3s = FIG-5196, L4 = FIG-5174,
L5 = FIG-5173, L6 = FIG-5175, L6b = FIG-5176, L7 = FIG-5177, L7b = FIG-5198,
L7p = FIG-5197, L8 = FIG-5178, L10a = FIG-5190, L10g = FIG-5200, L11 = FIG-5187,
L13 = FIG-5193.

## Seams

| Seam | Where | Contract | Real (I0) | Stubs owned by |
|---|---|---|---|---|
| S0 storage port carries domain rows | `lash-durable/src/domain/` (row types), `lash-durable/src/tx.rs` (`ActorTx::write`), `lash-store-sql/src/durable/*` (statements), `lash-sqlite-store/src/durable/*`, `lash-postgres-store/src/postgres/durable/*` | Domain writes apply in order inside the fenced owner commit, after the fence; conditional non-owner writes go through `MailTx`. No SQL outside the dialect modules (`scripts/check-durable-sql.py`). | `DomainWrite` dispatch and `DurableReads` delegation in both dialects; `FaultStore` forwarding (it cuts a commit that carries a domain write) | each row family's lane: turns and run records and snapshots V0, waits L5, processes and park events L6, session close L6b |
| S1 `ActorContext` is the one effect context | `lash-core-execution/src/runtime/actor/` | One concrete context per activation. No engine trait, no controller trait, no default bodies. Each `RuntimeEffectCommand` group has its own method; the caller picks it (`lash-core/src/runtime/turn_driver/issue.rs`). | `core.rs`: actor, epoch, clock, cancel, probe, `begin`/`commit`, `note_due`, `backend`, scopes, ordinals, `unavailable` (testing), `detached` | per group, see [the variant classification](#execute_effect-variant-classification) |
| S2 commit labels, config, dues, dispatch, replay probe | `lash-durable/src/{labels,durable_config,dues,dispatch,probe}.rs`, `lash-durable-test/src/tripwire.rs` | Every label the lanes emit is in `CommitLabel::ALL`; a lane that needs a new label adds it here in its own lane. `DurableConfig` is one validated struct whose field docs name their owners. A `waiting` release uses the minimum due. `ActorDispatch` routes by actor kind. `DurableProbe` has no default bodies. | all of it, with self-tests (`Tripwire` counts each event kind) | the session activation V0, the process activation L6 |
| S3 turn phases and the restore entry | `lash-core/src/runtime/durable/{session,session_mail}.rs` | Load rows, `restore_turn` (exactly one `restore_from_checkpoint`), re-deliver the pending effect, `run_phases`. Nothing re-executes orchestration to reach a recorded outcome. `drain_session_mail` runs first on every claim. | types | V0 then L3 (activation, admit, restore, phases), L3 (`request_turn_cancel`), L3s (`drain_session_mail`) |
| S4 admitted executions and tool rounds | `lash-core-execution/src/runtime/actor/round.rs` | No body starts before `admit`'s commit; the fold calls no producer; a started `Once` without an outcome folds to `Interrupted`; a `Repeatable` reruns at its ordinal; `(owner, run, ordinal)` is the second fence. | types | V0 (`admit`, `run_body`, `settle`, `fold`, the run-record family), L4 (rounds, retries, member cancel, `RoundTools`, in-place `tool_effect`) |
| S5 waits and completion keys | `lash-core-execution/src/runtime/actor/waits.rs`, `lash-durable/src/domain/waits.rs` | The deadline is written once at minting; the first resolution wins; host resolve refuses every kind but `tool_completion` and `custom` with `ReservedKind` and writes nothing; a resolution locks the wait row, then the actor row (an owner commit fences its actor row first, so a deadlock between them aborts one, which retries); every await races the awaiter's own `cancel` mail; a durable backend without secrets is refused (S9). | types, `CompletionKeySecrets` accessors | filled by L5 (`waits.rs`, `wait_effects.rs`, `lash_waits` in both dialects); the await-event methods L5 kept for their callers are L3's, L4's and L6's (see [left for owners](#left-for-owners)) |
| S6 host process engines are state machines | `lash-core-execution/src/runtime/process/{engine,engine_state}.rs`, `lash-core-execution/src/runtime/actor/process.rs` | `advance(state, event) -> (state, action)`; the new state and its action's admission commit in one `process.advance` transaction. Cancel is delivered once within the grace; at `grace_until` lash forces the terminal. No `run`, no `await_terminal`, no effect controller on the run context, no default bodies. | the trait and its types | L6 (every engine's `advance`, `end_scope`, the process activation, `process_effect`) |
| S7 VM snapshots and broker admission | `lash-vm-broker/src/snapshot.rs`, `lash-core-execution/src/runtime/actor/vm.rs`, `lash-durable/src/domain/{keys,snapshots}.rs` | The snapshot revision, its broker ledger, `admit` + `x_start` for every operation since the last snapshot and new waits commit in one `cell.snapshot+admit` transaction; restore injects saved outcomes by `OperationId`; nothing re-dispatches. | types; `SnapshotStore` replaces `CheckpointStore` | none: V0 and L7 filled them |
| S8 projection providers | `lashlang/src/runtime/projection_provider.rs`, `lashlang/src/runtime/value.rs` (`ResourceRef`), `lash-core-execution/src/runtime/actor/projection.rs` | Reads are pure, `Repeatable` and never recorded; a read answers `None` when its provider does not answer that request; a missing provider for a type found in a value or a snapshot is a typed refusal. | all of it: L7p (FIG-5197) filled the catalog, the VM's `ProjectionReader`, the worker's batched wire and the lash-provided `history` provider | none |
| S9 the durable backend | `lash-core-execution/src/backend.rs`; the facade's `DurableBackendBuilder` (`lash/src/durable.rs`) | `Backend` is concrete. Assembly refuses invalid settings, two engines of one kind, two providers of one projection type, and missing completion secrets. | `Backend::assemble`, accessors, `StoreSet::durable_store` in both dialects, `DurableBackendBuilder::build`; laws in `lash-conformance/src/backend_assembly_tests.rs` | L3s (`wake_session`), L6 (`wake_process`), L3 (`serve`), L5 (secret validation) |
| S10 table ownership | `lash-postgres-store/schema.sql` (with its regenerated `teardown.sql` and `schema-shape.txt`), `lash-sqlite-store/src/schema.rs` (`FRAGMENTS`) | See [table ownership](#table-ownership). | DDL for `lash_run_records`, `lash_exec_snapshots` | V0 (statements) |

### Renames and re-homes

- **S8:** the provider trait, its catalog and `ResourceRef` live in `lashlang`, beside the read types they answer (`ProjectedReadRequest`, `ProjectedReadResponse`), because the VM sits above `lash-core-execution`. The backend carries the built catalog behind `ProjectionProviders`, and the VM half reads it back as `lashlang::ProjectionCatalog`. L7p (FIG-5197) changed two pinned signatures, by the orchestrator's ruling: `ProjectionProvider::read` answers `Result<Option<ProjectedReadResponse>, ProjectionError>` and `read_range` `Result<Vec<Option<ProjectedReadResponse>>, ProjectionError>` (`None`: the provider does not answer that request, FIG-2863), with serde on `ProjectionRefusal` and `ProjectionError` so a refusal crosses the worker wire typed; and `lash_vm_broker::ParentEffects::projection` is `async`, so the parent awaits the provider. The VM reads synchronously through a `ProjectionReader` its `ProjectedBindings` carry (the worker's IPC wire, or the catalog in process), installed for every poll of the run loop; a projection value holds only `{ name, type_name, ResourceRef }`. `ProjectionCatalog::of_backend` reads a backend's catalog back, `ActorContext::projection_providers` and `RuntimeExecutionContext::projection_providers` reach it from a run, and `history` is the lash-provided provider each cell registers (`lash_protocol_rlm::HISTORY_PROJECTION`, refused as a host provider at build). A member read of a projection that yields a scalar is that plain value, so `finish`, a tool argument and a snapshot carry the value itself; a compound member stays a projection of its path, and only a projection handle, never a scalar read, crosses a VM exit as a handle (FIG-5197).
- **S7:** `ActorContext::snapshot_store(exec)` is `lash_vm_broker::DurableSnapshotStore::new(cx, exec)`: the broker crate sits above the context's crate, so the context cannot name its type. `ExecKey` lives in `lash-durable`'s domain keys.
- **S7, L7 (FIG-5177):** one stored form for every VM execution. A `Checkpoint` is `{ vm, ledger: BrokerLedger, host, end }`: the ledger names the operation the VM stands on (`PendingOperation`, with its `OperationId` once admitted, and its pinned waits), the host's own state rides as `host` (a cell's envelope), and a run's end is `end` (`RecordedEnd`), which replaces V0's `StoredSnapshot::Ended`. `LedgerSnapshot` is deleted. `SnapshotStore` is `commit_quiet_point(QuietPoint { checkpoint, admit, waits, with }) -> Committed`, `settle(OperationId, &Performed)`, `recover(&PendingOperation) -> Recovered`, `latest` and `open_frame`; the store mints the `OperationId` from `admit`'s `AdmittedId` inside the commit, commits a started `Once`'s `Interrupted` as `cell.inject` on restore, and prunes the run records of runs before the oldest one the ledger names. The broker drives it for a worker; `lash_vm_broker::cell::run_cell`, V0's in-process runner, drives the same store through `settle_output` and `recover_output`, which speak the round primitive's `BodyOutput`.
- **S6:** the engine's state-machine types are in `runtime/process/engine_state.rs`, beside the trait.
- **S6, engine steps (L7b, FIG-5198):** `StepRequest` is an enum: `Tool { step, tool, input }` runs a catalog tool, and `Engine { step, kind: EngineStepKind, input }` runs one of the engine's own step bodies. An engine declares its bodies at registration (`ProcessEngineRegistration::with_engine_steps`, the `EngineSteps` trait: `kinds`, then `run(EngineStepRun, cancel) -> SettledOutcome`). Each body runs `Repeatable`, so it must be a recomputation from its input. `EngineStepRun` carries the process, its recorded `engine_config`, the tool catalog, the activation's `now` and `clock`, and the backend's projection providers. `ProcessEngineRegistry::engine_steps` refuses an unknown engine, an engine with no bodies, or an undeclared kind with a typed `EngineStepRefusal`, before admission. The process activation hands both variants to the host's `ProcessSteps` (`admit`, `body`), which dispatches an engine step through `engine_steps` and refuses it as `StepRefusal::Engine`; an engine step's admission records the tool `<engine>/<kind>` (`StepRequest::admitted_tool`), an identity only, never a dispatch key. A `vm_run` hands the broker quiet points that commit nowhere and admit nothing: the activation commits the VM the run ends on, in the engine's state. `EngineEvent::StepSettled` carries a `SettledOutcome`, which is the attempt outcome plus the payload of its material (required exactly for `Completed` and `Failed`, refused otherwise with `SettledOutcomeRefusal`).
- **S6, idle and emit (L7b, FIG-5198; landed by L6):** `EngineAction::Idle` means no work and no deadline: only the next mailbox event reaches `advance`. `EngineAction::Emit { event_type, payload }` appends one process event in the same `process.advance` transaction, answered by `EngineEvent::Emitted`.
- **S6, lashlang (L7b, FIG-5198):** the lashlang engine's state is its VM snapshot plus the operation it parked on (`lash-lashlang-runtime/src/engine/`). `advance` answers `Steps([vm_run])`. `vm_run` runs the VM from the committed snapshot to its next quiet point and answers the new snapshot with the one operation the VM issued there. The operation leaves as its own action: tool steps, `timer` engine steps for aggregate timers, `Sleep`, `AwaitProcess`, `Idle` for a signal wait, or `Emit`. Its outcome feeds the next `vm_run` by operation number. Segment handover (`SegmentHandover`, `PersistedSegmentHandover`, `ProcessContinuationStore`, `ProcessRunOutcome::SegmentBoundary`, `ProcessEngineRunContext`, `lash_process_segment_handovers`) is deleted, along with the process side of the lashlang replay machinery (the process key namespace and the journaled signal wait). Durable worker-recovery accounting (`WorkerRecoveryStore`, `StoreSet::worker_recovery`, `lash_worker_recovery`, the vm-client recovery store) is deleted too: an execution's worker budget counts from zero per run.
- **S9:** assembly is `Backend::assemble(BackendParts)` in `lash-core-execution`, with `DurableBuildError` beside it (`MissingCompletionSecrets`, `DuplicateEngine`, `DuplicateProvider`, `InvalidConfig`), re-exported through `lash-core`. The facade's `DurableBackendBuilder::build` is a thin wrapper over it. `Backend::durable` takes the store set's durable store once, on first use, so a test store set that serves none is never asked.
- **S1:** the journal-era scaffolding that surviving code still calls (`DriveFrontier`, the command journal guard, the owner-step gate) moved from the deleted scoped controller to `runtime/actor/journal.rs`. It is documented for deletion by L4 and L7.
- **S1:** the dispatch from a command to its group method is `turn_driver::issue::issue_effect`, an explicit match in `lash-core`; each other caller names its group method directly.
- **S1:** `CompletionKeyPreparation`, the one survivor of `lash-core-effect`'s `await_event_resolver.rs`, moved to `completion_key.rs`.
- **S4:** `ExecutionDraft.limit` is L3a's (FIG-5171) `lash_sansio::ExecutionLimit`; I0 defines no copy.
- **S9, facade:** `DurableBackendBuilder::new` keeps the `Arc<dyn StoreSet>` that L10a's skeleton landed with, because every host already hands it one. `config` takes `DurableSettings` (the unvalidated parameters) rather than a `DurableConfig`, so `build` is where they are validated and `InvalidConfig` is reachable. `projection_provider` exists with the `rlm` feature, which brings `lashlang`; `build` registers the providers into a `lashlang::ProjectionCatalog` (whose `register` is L7p's) and maps a refused registration, or a host provider of the lash-provided `history` type, to `DuplicateProvider`. The facade re-exports the build vocabulary from `lash::durable` (`CompletionKeySecrets`, `DurableBuildError`, `DurableConfig`, `DurableSettings`, `DurableStore`, `KeyVersion`, `SecretBytes`, `SecretsRefusal`) and the engine state machine from `lash::plugins` (`EngineAction`, `EngineEvent`, `EngineState`, `EngineStateFormat`, `HostWaitKind`, `KeyName`, `StepName`, `StepRequest`), so an external engine and store set can be written against the facade alone.
- **S9, signals (L8, FIG-5178):** `StoreSet` gained `durable_signals() -> Option<Arc<dyn lash_durable::Signals>>`, agreed with the orchestrator. PostgreSQL answers `PostgresSignals` (after-commit `pg_notify` wake hints, a listener per node that holds the boot's session advisory lock, liveness probes and the reap of a boot whose lock is released); SQLite and the test store sets answer `None`, because one node per database keeps its wakes in process. `serve` passes it to `Runner::with_signals` when `DurableConfig`'s notifier is `AfterCommit`, and every mailbox commit through `Backend::commit_mail` hands what it woke to the shared `Hints`, which hint in process or publish.
- **S1, work ports:** `EffectEngine::{session_work, process_work}` became `DurableSessionWork` and `DurableProcessWork` (`runtime/work/durable.rs`), the facade core's session-work engine and process port over the backend. Session work is a producer's row and `Backend::wake_session` in one transaction (L3s), and a process start needs no delivery: registration creates its actor ready, and a trigger occurrence registers its processes in its own `trigger.start` transaction (L6); the shift ask, `schedule_shift` and `await_shift` are deleted, the terminal wait and its publication are L5's, and cancel delivery is L6's mail.
- **S0, L6b:** the session-close domain also records a session's turn scopes whose cascade is still marking (`SessionCloseWrite::ScopeEnding` and `ScopeEnded`), and `DurableReads` gained `session_close` and `ending_scopes`. A deletion is session mail (`session.close`, under `mail.session`); the close runs as the session actor's closing state in `lash-core/src/runtime/durable/session_close.rs`, and a turn's scope end, its cascade cursor work and the bounded wait for its children (G1b) are in `durable/turn_scope.rs`.
- **S1, perf:** the runtime-perf scenario `ScopedEffectController` is `ScopedEffects`; its report name `scoped_effect_controller` and its budget are unchanged.

## `execute_effect` variant classification

`execute_effect` is gone. Each `RuntimeEffectCommand` variant goes through its group's `ActorContext` method (the owner is binding), and each is classified as a write in its phase transaction, an admitted execution (S4), or deleted. The last column is I0's reading of ADR 0132 and the lane specs; an owner that finds a variant needs another shape changes the row here in its own lane and says why:

| Group method (owner) | Variant | Becomes |
|---|---|---|
| `turn_effect` (V0, then L3) | `BeforeLlmCall`, `AssistantResponseHooks` | phase-transaction write: the hook recomputes from committed state at its phase, and its result commits with the phase (L3) |
| | `LlmCall` | phase-transaction writes: `model.start` pins the call, `model.done` commits its answer |
| | `Direct` | admitted execution of the calling tool's body |
| | `SyncExecutionEnvironment`, `ResolveTurnConfig`, `RecordCompactionBase`, `RenderCompactionPrompt` | phase-transaction write (`turn.prepare`) |
| | `Checkpoint` | phase-transaction write (the bounded checkpoint ref on the turn row) |
| | `RecoverFollowOn` | deleted: restore reads the turn row (S3) |
| | `TraceBoundary` | phase-transaction write (the trace receipt rides the commit) |
| `session_effect` (L3) | `ResolveConfigTransaction`, `ReadSessionCommandRun` | phase-transaction write under the session epoch |
| | `CloseRunScope` | phase-transaction write (`turn.commit` ends the run's scope through `process::end_scope`) |
| | `BeginSessionClose` | deleted (L6b): a deletion is session mail, and the close is the session actor's closing state (`runtime/durable/session_close.rs`), one labelled transaction per step |
| `ingress_effect` (L3s) | `AcceptTurnInput`, `TransitionPlugins`, `PluginCallbacks` | run once by the producer or plugin host; the producer's row and the session's wake commit together, and `drain_session_mail` admits under the epoch |
| | `AdmitShift`, `DrawRunStart` | deleted with the shift (L3s) |
| | `ObserveDrainMark` | deleted: drain is a release at a committed phase (L11) |
| `tool_effect` (L4) | `ToolAttempt`, `PresentToolResult` | run in place, recorded nowhere: inside the admitted execution that runs the call (a round member between its `x_start` and `x_outcome`, or a code cell up to its next snapshot); a round's presentation record commits in `round.present+model.start` |
| | `RestoreRunMaterial` | deleted: the fold reads committed records (S4) |
| | `Trigger` | run in place until fig-5174-pending makes it a store-local effect of the call's settled outcome (`StoreLocalEffect::TriggerCreate`/`TriggerDelete`, F3) |
| | `IngestTriggerOccurrence`, `AdmitTriggerDelivery` | deleted: an emission records its occurrence, starts each delivery's process and binds it in one `trigger.start` mailbox commit (fig-5175-trigger) |
| `wait_effect` (L5) | `AwaitEvent`, `Sleep` | phase-transaction write: a pinned wait row or timer, raced against cancel mail |
| | `PeekAwaitEvent` | read of a wait row, recorded nowhere |
| `process_effect` (L6) | `Process` (start, signal, await, cancel) | store-local effect (start, signal), wait (await), mail (cancel) |
| | `LoadExecutionEnv` | admitted execution |
| `vm_effect` (V0, then L7) | `ExecCode` | admitted execution from the cell's snapshot, operations admitted at quiet points |
| | `LanguageRuntimeValue` | run in place, recorded nowhere: a cell resumed from its snapshot never asks again for a value its heap holds |

**L3 (FIG-5172):** `turn_effect` and `session_effect` run each command of their group in place and refuse every other. Nothing is journaled: a turn effect's result is durable only through the phase transaction that commits it, and a restore recomputes it from committed state; a session effect's runner makes its own idempotent store write.

## The fold (S1 rule 2)

Each constant default was deleted and its call sites folded to the constant:

| Method | Folded to | What went with it |
|---|---|---|
| `owns_commit_backpressure` | `false` | `requires_local_commit_admission` (`commit_admission.rs`); its callers reduce to `if !has_durable_store` |
| `hands_over_turns` | `false` | `cells_hand_over`, `end_at_segment_boundary`, `observe_drain_mark` and `ObserveDrainMarkRunner`; the turn-handover branch in `session_init.rs` collapsed to the spawn path |
| `attempt_observation` | `None` | the observation plumbing in `direct.rs` and `trace/*` |
| `wants_segment_boundary` | `None` | the `LlmCall` boundary check in `machine.rs`, the boundary block in `handlers.rs`, the lashlang process segment-boundary closure (now `|| false`, reason `HandOver`) |
| `peek_run_cut` | `None` | L4 (FIG-5174) then deleted the cut plumbing it fed: `continuation.rs`, `CutChecked`/`CutRetained` and the Run transfer |
| `turn_attach` | `None` | `TurnControlAttachment::Attached` and `run_scoped`; only the resolver path remains |
| `bind_process_registry` | no-op | `ProcessRegistryBinding`, `ProcessRegistrationProbe` and both stores' registration probes; `bind_effect_host` on the registry and deployment store; `ProcessScopeFenceHosts` |

Also deleted with the traits: the remote-effect runner and its forwarding, `serve_effect_controller_task_request`, `UnavailableEffectController` (replaced by `ActorContext::unavailable()`), the turn-cancel closure owner in both stores, `retire_effect_journal` and `SessionDeleteFailure::Journal`, `LayeredEngine` and the layered effect host (the store decorator `LayeredBackend` stays), the attempt sentinel, and the build-generation binding in the shift (deleted with the shift by L3s).

Behaviours the deleted `execute_effect` wrapper added around every effect, which each group's owner re-establishes in its method: scope validation, the command journal guard, plugin publication, and wait receipts (deleted by L5 with `lash_wait_receipts`: a wait is its own row).

### Left for owners

Code the fold made unreachable, or nearly, that an owning lane is about to rewrite. I0 stopped here:

- **L4:** the journal guard and owner-step gate in `runtime/actor/journal.rs`. (L4 deleted the generation-cut plumbing and the Run transfer.)
- **L6 and L7b:** the `with_turn_hand_over(false)` plumbing (4 sites) and segment handover as process state; the process run-context builder (`process_runners/mod.rs`) and the capability items only it read (`session_runtime_store`, `execution_owner`, `turn_phase_probe`), kept under `#[expect(dead_code)]` for the advance-driven engine drive; `ProcessEngineRunContext` without effect accessors, and the lashlang run path (`run_lashlang_process`, which takes the context it will run under).
- **L3, L4 and L6:** the await-event methods addressed by the retired `AwaitEventKey` (`await_event_key`, `resolve_await_event`, `publish_await_event`, `peek_await_event`, `await_await_event`, `prepare_completion_key`, `wait_effect`'s `AwaitEvent` and `PeekAwaitEvent`, and `completion_host_key`), kept by L5 in `runtime/actor/await_event_legacy.rs`. Each reaches `port_pending`, whose arm names the lane by wait identity: the plugin task cancel signal L3 (turn control's waits are deleted: a turn cancel is session mail), tool completion and custom L4, process signals L6. They are deleted with their callers' ports; no wait row serves a recomputable key.
- At the L10a rebase I0 ported every file that still named the deleted seam, deleted `replay_read_gate.rs` (its subject was replay paths) and the pending list, and dropped Rule 7's engine-crate exclusion; L10a itself removed `RecordedJournal` and `read_recorded_journal`.

## Table ownership

Created in DDL by the lane named; written by the lanes in the last column. On SQLite the tables live in the durable core database, which L1b (FIG-5195) makes the deployment's only file; on PostgreSQL in `schema.sql` with its regenerated artifacts.

| Table or columns | DDL | Statements |
|---|---|---|
| `nodes`, `actors`, `actor_mail` | L1 (landed) | L1; `actors.parked` state, `park_json`, `failed_activations`, progress counters: L6; `nodes.draining`: L11 |
| turn phase state (columns on the run row or a 1:1 side table, one-unfinished-run unique index) | V0 | V0, then L3 |
| `lash_run_records` | **I0** | V0, then L4 |
| `lash_exec_snapshots` (CAS on `rev`; no `exported_descriptors` column) | **I0** | V0, then L7 (L6 writes `p/<pid>` through S7) |
| `lash_waits` (with `key_version`, `created_epoch`) | L5 | L5 |
| process actor columns, engine-state pointer, `cancel_requested_at_ms`, cascade cursor | L6 | L6 |
| `lash_park_events` | L6 | L6 |
| `session_close` (a closing session's last step, then its tombstone), `session_scope_ends` (a session's turn scopes whose cascade is still marking) | L6b | L6b |
| format columns, fleet format | L11 | L11 |
| notifier, advisory lock (no tables; the liveness probe and the released-boot reap read and write `nodes` and `actors` through the PostgreSQL engine module) | none | L8 |
| drops of replaced tables | the lane that replaces them | L10b squashes the 1.0 baseline last |

## Stub inventory

Generated from the tree with `scripts/check-substrate-todos.py`'s scanner; each lane removes its rows as it fills them.

Counts: L3 1, L4 1, L6 1 (3 in all).

### V0 (FIG-5170)

None: V0 filled its stubs. It re-tagged the journal-era ones its path never reaches: `turn_effect` to L3, and `record_run_record`, `start_run_record`, `start_run_attempt` and `start_run_prepare` to L4.

### L3 (FIG-5172)

| Where | Function | Stub |
|---|---|---|
| `crates/lash-core-execution/src/runtime/actor/await_event_legacy.rs` | `port_pending` | delete with the plugin task cancel signal's port to session mail |

### L4 (FIG-5174)

| Where | Function | Stub |
|---|---|---|
| `crates/lash-core-execution/src/runtime/actor/await_event_legacy.rs` | `port_pending` | delete with the tool completion keys' port to L5's pin, race and resolve_host (fig-5174-pending) |

### L6 (FIG-5175)

| Where | Function | Stub |
|---|---|---|
| `crates/lash-core-execution/src/runtime/actor/await_event_legacy.rs` | `port_pending` | delete with the process signals' port to process mail and L5's pin and race |

L6 filled the rest. The one row left was reached only by the lashlang run path's `await_process_signal_event`, which L7b (FIG-5198) deleted: an engine receives a signal as `EngineEvent::Signal` from its mail. No production caller reaches it; it goes with `AwaitEventWaitIdentity::ProcessSignal`, which only witnesses still construct.

### L6b (FIG-5176)

None: L6b filled the session-close domain's `apply` on SQLite and PostgreSQL, and its laws run on L6's process API.

### L7 (FIG-5177)

None: L7 filled `vm_effect` and `open_frame`. A VM effect runs in place and is recorded nowhere: a cell is durable through its snapshot, and a `LanguageRuntimeValue` is computed where it is asked.
