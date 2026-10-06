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
| S4 admitted executions and tool rounds | `lash-core-execution/src/runtime/actor/round.rs` | No body starts before `admit`'s commit; the fold calls no producer; a started `Once` without an outcome folds to `Interrupted`; a `Repeatable` reruns at its ordinal; `(owner, run, ordinal)` is the second fence. | types | V0 (`admit`, `run_body`, `settle`, `fold`, the run-record family), L4 (rounds, retries, sources, realization, `tool_effect`) |
| S5 waits and completion keys | `lash-core-execution/src/runtime/actor/waits.rs`, `lash-durable/src/domain/waits.rs` | The deadline is written once at minting; the first resolution wins; host resolve refuses every kind but `tool_completion` and `custom` with `ReservedKind` and writes nothing; lock order is wait row, then actor row; a durable backend without secrets is refused (S9). | types, `CompletionKeySecrets` accessors | L5 |
| S6 host process engines are state machines | `lash-core-execution/src/runtime/process/{engine,engine_state}.rs`, `lash-core-execution/src/runtime/actor/process.rs` | `advance(state, event) -> (state, action)`; the new state and its action's admission commit in one `process.advance` transaction. Cancel is delivered once within the grace; at `grace_until` lash forces the terminal. No `run`, no `await_terminal`, no effect controller on the run context, no default bodies. | the trait and its types | L6 (every engine's `advance`, `end_scope`, the process activation, `process_effect`) |
| S7 VM snapshots and broker admission | `lash-vm-broker/src/snapshot.rs`, `lash-core-execution/src/runtime/actor/vm.rs`, `lash-durable/src/domain/{keys,snapshots}.rs` | The snapshot revision, its broker ledger, `admit` + `x_start` for every operation since the last snapshot and new waits commit in one `cell.snapshot+admit` transaction; restore injects saved outcomes by `OperationId`; nothing re-dispatches. | types; `SnapshotStore` replaces `CheckpointStore` | V0 (`commit_quiet_point`, `latest`, `outcomes_to_inject`), L7 (`open_frame`, `vm_effect`) |
| S8 projection providers | `lashlang/src/runtime/projection_provider.rs`, `lashlang/src/runtime/value.rs` (`ResourceRef`), `lash-core-execution/src/runtime/actor/projection.rs` | Reads are pure, `Repeatable` and never recorded; a read answers `None` when its provider does not answer that request; a missing provider for a type found in a value or a snapshot is a typed refusal. | all of it: L7p (FIG-5197) filled the catalog, the VM's `ProjectionReader`, the worker's batched wire and the lash-provided `history` provider | none |
| S9 the durable backend | `lash-core-execution/src/backend.rs`; the facade's `DurableBackendBuilder` (`lash/src/durable.rs`) | `Backend` is concrete. Assembly refuses invalid settings, two engines of one kind, two providers of one projection type, and missing completion secrets. | `Backend::assemble`, accessors, `StoreSet::durable_store` in both dialects, `DurableBackendBuilder::build`; laws in `lash-conformance/src/backend_assembly_tests.rs` | L3s (`wake_session`), L6 (`wake_process`), L3 (`serve`), L5 (secret validation) |
| S10 table ownership | `lash-postgres-store/schema.sql` (with its regenerated `teardown.sql` and `schema-shape.txt`), `lash-sqlite-store/src/schema.rs` (`FRAGMENTS`) | See [table ownership](#table-ownership). | DDL for `lash_run_records`, `lash_exec_snapshots` | V0 (statements) |

### Renames and re-homes

- **S8:** the provider trait, its catalog and `ResourceRef` live in `lashlang`, beside the read types they answer (`ProjectedReadRequest`, `ProjectedReadResponse`), because the VM sits above `lash-core-execution`. The backend carries the built catalog behind `ProjectionProviders`, and the VM half reads it back as `lashlang::ProjectionCatalog`. L7p (FIG-5197) changed two pinned signatures, by the orchestrator's ruling: `ProjectionProvider::read` answers `Result<Option<ProjectedReadResponse>, ProjectionError>` and `read_range` `Result<Vec<Option<ProjectedReadResponse>>, ProjectionError>` (`None`: the provider does not answer that request, FIG-2863), with serde on `ProjectionRefusal` and `ProjectionError` so a refusal crosses the worker wire typed; and `lash_vm_broker::ParentEffects::projection` is `async`, so the parent awaits the provider. The VM reads synchronously through a `ProjectionReader` its `ProjectedBindings` carry (the worker's IPC wire, or the catalog in process), installed for every poll of the run loop; a projection value holds only `{ name, type_name, ResourceRef }`. `ProjectionCatalog::of_backend` reads a backend's catalog back, `ActorContext::projection_providers` and `RuntimeExecutionContext::projection_providers` reach it from a run, and `history` is the lash-provided provider each cell registers (`lash_protocol_rlm::HISTORY_PROJECTION`, refused as a host provider at build).
- **S7:** `ActorContext::snapshot_store(exec)` is `lash_vm_broker::DurableSnapshotStore::new(cx, exec)`: the broker crate sits above the context's crate, so the context cannot name its type. `ExecKey` lives in `lash-durable`'s domain keys.
- **S6:** the engine's state-machine types are in `runtime/process/engine_state.rs`, beside the trait.
- **S9:** assembly is `Backend::assemble(BackendParts)` in `lash-core-execution`, with `DurableBuildError` beside it (`MissingCompletionSecrets`, `DuplicateEngine`, `DuplicateProvider`, `InvalidConfig`), re-exported through `lash-core`. The facade's `DurableBackendBuilder::build` is a thin wrapper over it. `Backend::durable` takes the store set's durable store once, on first use, so a test store set that serves none is never asked.
- **S1:** the journal-era scaffolding that surviving code still calls (`DriveFrontier`, the command journal guard, the owner-step gate) moved from the deleted scoped controller to `runtime/actor/journal.rs`. It is documented for deletion by L4 and L7.
- **S1:** the dispatch from a command to its group method is `turn_driver::issue::issue_effect`, an explicit match in `lash-core`; each other caller names its group method directly.
- **S1:** `CompletionKeyPreparation`, the one survivor of `lash-core-effect`'s `await_event_resolver.rs`, moved to `completion_key.rs`.
- **S4:** `ExecutionDraft.limit` is L3a's (FIG-5171) `lash_sansio::ExecutionLimit`; I0 defines no copy.
- **S9, facade:** `DurableBackendBuilder::new` keeps the `Arc<dyn StoreSet>` that L10a's skeleton landed with, because every host already hands it one. `config` takes `DurableSettings` (the unvalidated parameters) rather than a `DurableConfig`, so `build` is where they are validated and `InvalidConfig` is reachable. `projection_provider` exists with the `rlm` feature, which brings `lashlang`; `build` registers the providers into a `lashlang::ProjectionCatalog` (whose `register` is L7p's) and maps a refused registration, or a host provider of the lash-provided `history` type, to `DuplicateProvider`. The facade re-exports the build vocabulary from `lash::durable` (`CompletionKeySecrets`, `DurableBuildError`, `DurableConfig`, `DurableSettings`, `DurableStore`, `KeyVersion`, `SecretBytes`, `SecretsRefusal`) and the engine state machine from `lash::plugins` (`EngineAction`, `EngineEvent`, `EngineState`, `EngineStateFormat`, `HostWaitKind`, `KeyName`, `StepName`, `StepRequest`), so an external engine and store set can be written against the facade alone.
- **S1, work ports:** `EffectEngine::{session_work, process_work}` became `DurableSessionWork` and `DurableProcessWork` (`runtime/work/durable.rs`), the facade core's session-work engine and process port over the backend. A shift ask is `Backend::wake_session` (L3s) and a process start is `Backend::wake_process` (L6); `schedule_shift` and `await_shift` are L3s stubs, the terminal wait and its publication are L5's, and cancel delivery is L6's mail. `install_session_shifts` is real (get-or-init).
- **S1, drain:** `drain_generation` marks the generation and wakes each live process (`Backend::wake_process`); turns no longer hand over at a drain (`hands_over_turns` folded to false), so the session loop is gone. `generation_drain_status` reads `NoDeployments` for the deleted engine deployment registry; L10g deletes the rest of the generation machinery. The facade core composes its build generation from its plugins itself instead of binding it on the backend.
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
| | `BeginSessionClose` | phase-transaction write (L6b's close steps) |
| `shift_effect` (L3s) | `AdmitShift`, `DrawRunStart`, `AcceptTurnInput`, `TransitionPlugins`, `PluginCallbacks` | phase-transaction write in the session mail drain (`turn.accept`, `turn.admit`) |
| | `ObserveDrainMark` | deleted: drain is a release at a committed phase (L11) |
| `tool_effect` (L4) | `ToolAttempt` | admitted execution |
| | `PresentToolResult` | phase-transaction write (`round.present+model.start`) |
| | `RestoreRunMaterial` | deleted: the fold reads committed records (S4) |
| | `Trigger`, `IngestTriggerOccurrence`, `AdmitTriggerDelivery` | store-local effect of the call's settled outcome (`StoreLocalEffect::TriggerCreate`/`TriggerDelete`) |
| `wait_effect` (L5) | `AwaitEvent`, `Sleep` | phase-transaction write: a pinned wait row or timer, raced against cancel mail |
| | `PeekAwaitEvent` | read of a wait row, recorded nowhere |
| `process_effect` (L6) | `Process` (start, signal, await, cancel) | store-local effect (start, signal), wait (await), mail (cancel) |
| | `LoadExecutionEnv` | admitted execution |
| `vm_effect` (V0, then L7) | `ExecCode` | admitted execution from the cell's snapshot, operations admitted at quiet points |
| | `LanguageRuntimeValue` | deleted: values are snapshotted with the VM heap |

## The fold (S1 rule 2)

Each constant default was deleted and its call sites folded to the constant:

| Method | Folded to | What went with it |
|---|---|---|
| `owns_commit_backpressure` | `false` | `requires_local_commit_admission` (`commit_admission.rs`); its callers reduce to `if !has_durable_store` |
| `hands_over_turns` | `false` | `cells_hand_over`, `end_at_segment_boundary`, `observe_drain_mark` and `ObserveDrainMarkRunner`; the turn-handover branch in `session_init.rs` collapsed to the spawn path |
| `attempt_observation` | `None` | the observation plumbing in `direct.rs` and `trace/*` |
| `wants_segment_boundary` | `None` | the `LlmCall` boundary check in `machine.rs`, the boundary block in `handlers.rs`, the lashlang process segment-boundary closure (now `|| false`, reason `HandOver`) |
| `peek_run_cut` | `None` | `continuation.rs` `generation_cut_entry` answers `None`; `drain.rs` folds the cut to `None` |
| `turn_attach` | `None` | `TurnControlAttachment::Attached` and `run_scoped`; only the resolver path remains |
| `bind_process_registry` | no-op | `ProcessRegistryBinding`, `ProcessRegistrationProbe` and both stores' registration probes; `bind_effect_host` on the registry and deployment store; `ProcessScopeFenceHosts` |

Also deleted with the traits: the remote-effect runner and its forwarding, `serve_effect_controller_task_request`, `UnavailableEffectController` (replaced by `ActorContext::unavailable()`), the turn-cancel closure owner in both stores, `retire_effect_journal` and `SessionDeleteFailure::Journal`, `LayeredEngine` and the layered effect host (the store decorator `LayeredBackend` stays), the attempt sentinel, and the build-generation binding in the shift (`admitting_generation` refuses `GenerationUnbound` until L3s replaces it).

Behaviours the deleted `execute_effect` wrapper added around every effect, which each group's owner re-establishes in its method: scope validation, the command journal guard, plugin publication, and wait receipts (`trace/wait_receipts.rs`, kept for L5 under `#[expect(dead_code)]`).

### Left for owners

Code the fold made unreachable, or nearly, that an owning lane is about to rewrite. I0 stopped here:

- **L4:** the generation-cut plumbing in the run coordinator (`observe_generation_cuts`, `generation_cut_entry`, `check_cut`, `CutChecked`); the journal guard and owner-step gate in `runtime/actor/journal.rs`.
- **L6 and L7b:** the `with_turn_hand_over(false)` plumbing (4 sites) and segment handover as process state; the process run-context builder (`process_runners/mod.rs`) and the capability items only it read (`session_runtime_store`, `execution_owner`, `turn_phase_probe`), kept under `#[expect(dead_code)]` for the advance-driven engine drive; `ProcessEngineRunContext` without effect accessors, and the lashlang run path (`run_lashlang_process`, which takes the context it will run under).
- **L3 and L5:** the turn-control promise machinery, which compiles against L3's and L5's stubs.
- **V0:** `RunRecordObserver::bind`, kept for `record_run_record`.
- **L6:** host and test process engines whose `advance` is a stub (`IngressAdmissionEngine`, `PayloadGatedEngine`, the artifact-cleanup engine, the h2 receiver): their tests compile and reach the stub until L6 ports the engine drive.
- At the L10a rebase I0 ported every file that still named the deleted seam, deleted `replay_read_gate.rs` (its subject was replay paths) and the pending list, and dropped Rule 7's Restate exclusion; L10a itself removed `RecordedJournal` and `read_recorded_journal`.

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
| session closing state | L6b | L6b |
| format columns, fleet format | L11 | L11 |
| notifier, advisory lock (no tables) | none | L8 |
| drops of replaced tables | the lane that replaces them | L10b squashes the 1.0 baseline last |

## Stub inventory

Generated from the tree with `scripts/check-substrate-todos.py`'s scanner; each lane removes its rows as it fills them.

Counts: L3 9, L3s 5, L4 15, L5 35, L6 32, L6b 2, L7 2 (100 in all).

### V0 (FIG-5170)

None: V0 filled its stubs. It re-tagged the journal-era ones its path never reaches: `turn_effect` to L3, and `record_run_record`, `start_run_record`, `start_run_attempt` and `start_run_prepare` to L4.

### L3 (FIG-5172)

| Where | Function | Stub |
|---|---|---|
| `crates/lash-core-execution/src/runtime/actor/turn.rs` | `register_turn_cancel_closure_participant` | delete with the turn-control binding tables once cancel is mail |
| `crates/lash-core-execution/src/runtime/actor/turn.rs` | `release_turn_cancel_closure_participant` | delete with the turn-control binding tables once cancel is mail |
| `crates/lash-core-execution/src/runtime/actor/turn.rs` | `session_effect` | run a session effect as a write under the session's epoch |
| `crates/lash-core-execution/src/runtime/actor/turn.rs` | `turn_control_binding` | replace the turn-control binding by turn cancel mail |
| `crates/lash-core-execution/src/runtime/actor/turn.rs` | `turn_control_binding_id` | replace the turn-control binding by turn cancel mail |
| `crates/lash-core-execution/src/runtime/actor/turn.rs` | `turn_effect` | run a turn effect inside its phase transaction |
| `crates/lash-core/src/runtime/durable/session.rs` | `request_turn_cancel` | request a turn cancel as session mail |
| `crates/lash-postgres-store/src/postgres/durable/turns.rs` | `request_cancel` | record a turn cancel request and wake the session on PostgreSQL |
| `crates/lash-sqlite-store/src/durable/turns.rs` | `request_cancel` | record a turn cancel request and wake the session on SQLite |

### L3s (FIG-5196)

| Where | Function | Stub |
|---|---|---|
| `crates/lash-core-execution/src/backend.rs` | `wake_session` | wake a session actor in a mailbox transaction |
| `crates/lash-core-execution/src/runtime/actor/shift.rs` | `shift_effect` | apply a shift effect under the session's epoch, or delete it with the shift fence |
| `crates/lash-core-execution/src/runtime/work/durable.rs` | `await_shift` | answer once the session actor's activation for the ask released |
| `crates/lash-core-execution/src/runtime/work/durable.rs` | `schedule_shift` | wake the session actor; its activation drains the session's mail |
| `crates/lash-core/src/runtime/durable/session_mail.rs` | `drain_session_mail` | drain pending inputs, queued work, control intents and plugin transitions under the epoch |

### L4 (FIG-5174)

| Where | Function | Stub |
|---|---|---|
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `admit_round` | admit a tool round inside model.done |
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `arm_run_source` | arm a Run source as a pinned wait |
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `attach_run_process_terminal` | attach a process-terminal source as a process_terminal wait |
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `attach_run_realization` | read a realization from its call's committed outcome |
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `await_run_sources` | race Run sources through waits::race |
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `cancel_run_source` | cancel a Run source, first resolution wins |
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `issue_run_realization` | realize an intent as a store-local effect of its call's outcome |
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `present` | present a round in declared order from its committed records |
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `record_run_record` | record a Run record as a run_records row |
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `select_run_sources` | select the first completed Run source |
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `start_run_attempt` | start a Run attempt through admit, run_body and settle |
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `start_run_prepare` | start a declared-start preparation as an admitted execution |
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `start_run_record` | start a Run record as an admitted execution |
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `start_run_retry` | record a retry with its due time and register the due source |
| `crates/lash-core-execution/src/runtime/actor/round.rs` | `tool_effect` | run a tool-round effect as an admitted execution or a round write |

### L5 (FIG-5173)

| Where | Function | Stub |
|---|---|---|
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `await_await_event` | await a wait through race |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `await_event_authority_binding_id` | delete with await-event keys; wait keys are HMAC-minted |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `await_event_key` | mint a completion key by pin |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `await_event_scope_is_retired` | delete with scope retirement fences; revocation is per wait row |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `await_external` | await a host-resolvable wait, bounded by its deadline |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `await_process` | await a process terminal, bounded and cancellable |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `cancel_await_events_for_session` | revoke a session's waits through revoke_scope |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `list_outstanding_await_event_keys` | list a session's pending host-resolvable waits |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `new` | validate versioned completion secrets |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `peek_await_event` | read a wait row |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `pin` | mint a wait row and its HMAC key in the owner's transaction |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `prepare_completion_key` | mint a completion key by pin |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `publish_await_event` | resolve a wait through resolve_host |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `race` | race pinned waits against the awaiter's cancel mail |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `reinstate_await_event_scope` | delete with scope retirement fences; revocation is per wait row |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `resolve` | resolve a wait deadline under its default and ceiling |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `resolve_await_event` | resolve a wait through resolve_host |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `resolve_host` | verify a host key and resolve its wait, first winner |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `resolve_process_terminal_waits` | resolve a process's terminal waits in its terminal transaction |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `retire_await_events_for_scope` | revoke a scope's waits through revoke_scope |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `retire_await_events_for_scope_if_quiescent` | revoke a scope's waits through revoke_scope |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `retire_closed_run_waits` | revoke a closed run's waits through revoke_scope |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `revoke_await_events_for_session` | revoke a session's waits through revoke_scope |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `revoke_scope` | revoke a scope's pending waits |
| `crates/lash-core-execution/src/runtime/actor/waits.rs` | `wait_effect` | run a wait effect as a pinned wait or timer, raced against cancel |
| `crates/lash-core-execution/src/runtime/work/durable.rs` | `await_process_terminal` | await the process's terminal through a process-terminal wait row |
| `crates/lash-core-execution/src/runtime/work/durable.rs` | `publish_process_terminal` | resolve the process's terminal waits in its terminal transaction |
| `crates/lash-postgres-store/src/postgres/durable/waits.rs` | `apply` | pin, settle or revoke waits on PostgreSQL |
| `crates/lash-postgres-store/src/postgres/durable/waits.rs` | `pending` | read an actor's pending waits on PostgreSQL |
| `crates/lash-postgres-store/src/postgres/durable/waits.rs` | `resolve` | resolve a wait from pending, first winner, and wake its owner on PostgreSQL |
| `crates/lash-postgres-store/src/postgres/durable/waits.rs` | `wait` | read one wait on PostgreSQL |
| `crates/lash-sqlite-store/src/durable/waits.rs` | `apply` | pin, settle or revoke waits on SQLite |
| `crates/lash-sqlite-store/src/durable/waits.rs` | `pending` | read an actor's pending waits on SQLite |
| `crates/lash-sqlite-store/src/durable/waits.rs` | `resolve` | resolve a wait from pending, first winner, and wake its owner on SQLite |
| `crates/lash-sqlite-store/src/durable/waits.rs` | `wait` | read one wait on SQLite |

### L6 (FIG-5175)

| Where | Function | Stub |
|---|---|---|
| `crates/lash-conformance/src/conformance/bound_trigger_duplicate.rs` | `advance` | port IncrementEngine to advance |
| `crates/lash-conformance/src/conformance/definitions.rs` | `advance` | port ModuleDefinitionEngine to advance |
| `crates/lash-conformance/src/conformance/obligation_relay.rs` | `advance` | port MissingCarryEngine to advance |
| `crates/lash-conformance/src/conformance/process_prune_start_staging.rs` | `advance` | port ModuleNamingEngine to advance |
| `crates/lash-conformance/src/conformance/process_trigger_retention.rs` | `advance` | port TriggerTargetEngine to advance |
| `crates/lash-core-execution/src/backend.rs` | `wake_process` | wake a process actor in a mailbox transaction |
| `crates/lash-core-execution/src/runtime/actor/process.rs` | `end_scope` | mark a batch of a scope's Until children for cancel |
| `crates/lash-core-execution/src/runtime/actor/process.rs` | `observe_process_cancel` | read the process's cancel_requested_at, or delete where advance replaces it |
| `crates/lash-core-execution/src/runtime/actor/process.rs` | `process_effect` | run a process effect as a store-local effect, wait or mail |
| `crates/lash-core-execution/src/runtime/actor/process.rs` | `record_process_drive_step` | delete where advance replaces it, else a write under the process epoch |
| `crates/lash-core-execution/src/runtime/process/definition_tests.rs` | `advance` | port SignedEngine to advance |
| `crates/lash-core-execution/src/runtime/process/engine_resolve_tests.rs` | `advance` | port SignedEngine to advance |
| `crates/lash-core-execution/src/runtime/work/durable.rs` | `deliver_cancel` | request the process's cancel as mail; the first request wins |
| `crates/lash-core-execution/src/testing/mod.rs` | `advance` | port FixtureProcessEngine to advance |
| `crates/lash-core-execution/src/testing/mod.rs` | `advance` | port HeldProcessEngine to advance |
| `crates/lash-core/src/runtime/artifact_cleanup_tests.rs` | `advance` | port Engine to advance |
| `crates/lash-core/src/runtime/session_manager/process_runners/runner.rs` | `run_admitted_process` | drive the engine's process by advance from the process activation |
| `crates/lash-core/tests/runtime_support/payload_gated_engine.rs` | `advance` | port PayloadGatedEngine to advance |
| `crates/lash-lashlang-runtime/src/lib.rs` | `advance` | port LashlangProcessEngine to advance |
| `crates/lash-postgres-store/src/postgres/durable/park_events.rs` | `apply` | append to the park feed on PostgreSQL |
| `crates/lash-postgres-store/src/postgres/durable/park_events.rs` | `read` | read the park feed on PostgreSQL |
| `crates/lash-postgres-store/src/postgres/durable/processes.rs` | `apply` | register, advance, end or cascade a process on PostgreSQL |
| `crates/lash-postgres-store/src/postgres/durable/processes.rs` | `live_until_descendants` | list a scope's live Until descendants on PostgreSQL |
| `crates/lash-postgres-store/src/postgres/durable/processes.rs` | `process` | read a process actor's row on PostgreSQL |
| `crates/lash-postgres-store/src/postgres/durable/processes.rs` | `request_cancel` | record a process's first cancel request and control-wake it on PostgreSQL |
| `crates/lash-sqlite-store/src/durable/park_events.rs` | `apply` | append to the park feed on SQLite |
| `crates/lash-sqlite-store/src/durable/park_events.rs` | `read` | read the park feed on SQLite |
| `crates/lash-sqlite-store/src/durable/processes.rs` | `apply` | register, advance, end or cascade a process on SQLite |
| `crates/lash-sqlite-store/src/durable/processes.rs` | `live_until_descendants` | list a scope's live Until descendants on SQLite |
| `crates/lash-sqlite-store/src/durable/processes.rs` | `process` | read a process actor's row on SQLite |
| `crates/lash-sqlite-store/src/durable/processes.rs` | `request_cancel` | record a process's first cancel request and control-wake it on SQLite |
| `crates/lash/src/tests/tool_intent_ingress.rs` | `advance` | port IngressAdmissionEngine to advance |
| `examples/shared/h2_receiver.rs` | `advance` | port ReceiverEngine to advance: run until cancelled |

### L6b (FIG-5176)

| Where | Function | Stub |
|---|---|---|
| `crates/lash-postgres-store/src/postgres/durable/session_close.rs` | `apply` | record a session close step on PostgreSQL |
| `crates/lash-sqlite-store/src/durable/session_close.rs` | `apply` | record a session close step on SQLite |

### L7 (FIG-5177)

| Where | Function | Stub |
|---|---|---|
| `crates/lash-core-execution/src/runtime/actor/vm.rs` | `vm_effect` | run a cell from its snapshot, admitting its operations at quiet points |
| `crates/lash-vm-broker/src/snapshot.rs` | `open_frame` | open a frame, dropping earlier frames' snapshots |
