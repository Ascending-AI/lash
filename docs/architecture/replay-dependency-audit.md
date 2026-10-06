# Replay-dependency audit (V0, FIG-5170)

ADR 0132 §2 forbids re-running code against a recorded history, by position or by key (NR-1 to NR-4). This audit lists every site where today's turn + cell + `Once` tool path relied on replay, and says what replaces it. It is the evidence for V0's criterion P8.

Each row has one class:

- **covered:** a commit label makes the site state. V0's law observes it in execution where the row says so.
- **pure:** recomputation from committed state. It runs again, but nothing recorded is served to it.
- **needs a phase record:** state-first, but the record is not yet written. The row names the lane that adds it.
- **blocker:** it cannot be made state without re-running code.

**Result: zero blockers.**

## What V0 executed, and what only this audit covers

`crates/lash-durable-test/tests/vertical_crash_proof.rs` runs one turn end to end on simulated nodes A and B. Every piece below is production code:

- the session activation (`lash-core/src/runtime/durable/session.rs`);
- the `TurnMachine` with `checkpoint` / `restore_from_checkpoint`;
- a TypeScript cell lowered by `lash-typescript` and run on the lashlang VM, through `lash-vm-broker::cell::run_cell` and the `DurableSnapshotStore`;
- the admitted-execution primitive (`round::{admit, run_body, settle, fold}`);
- the L1 store on SQLite memory, SQLite file and PostgreSQL;
- the L2 harness (`FaultStore`, `SimClock`, `SimNodes`, `Matrix`, `Tripwire`).

The protocol driver and the model are scripted in the test. The cell's host operation `ext.write` is a test `CellOperations`.

The following were not executed and are covered here only:

- the production turn driver effect loop;
- the turn loop;
- the shift;
- the `RunCoordinator`;
- the RLM protocol driver;
- the worker-process broker and the lashlang worker run path (`worker_execution.rs`, `replay_run.rs`).

### The V0 commit labels

| Label | One transaction holding | What it makes state |
|---|---|---|
| `turn.admit` | the `session_runs` row, the `turn_phases` row (phase `model`, no checkpoint), and the admission mail acknowledged | the turn and its admission (base head, messages) |
| `model.start` | the phase advance with the machine checkpoint and `ModelPin{attempt, request_ref, deadline}` | the transcript up to the call and the pinned attempt. A re-delivery is a new attempt (attempt + 1), never a served answer |
| `cell.snapshot+admit` | the turn checkpoint advanced to `tools`, snapshot rev 1 (VM bytes + broker ledger), the run's `admit` row and the operation's `x_start` | the cell parked on its one operation, admitted and started. The fusion keeps the turn from restoring to before the snapshot |
| `round.outcome` | the operation's `x_outcome` row with its material | the body's answer, or `Interrupted` folded on recovery |
| `cell.snapshot` | snapshot rev 2: `Ended{result}` | the cell's result; a later re-delivery reads it and enters no VM |
| `model.start` (second) | the checkpoint after the cell's observation, the new pin | the cell result inside the transcript |
| `turn.commit` | the head CAS (`expected` → `expected + 1`), the revision, and the terminal | the turn's answer, published once (P7) |
| `session.release` | the actor released to `idle` | nothing more to run |

## Named sites

| # | Site | How it relied on replay | Class | What replaces it |
|---|---|---|---|---|
| 1 | Turn driver effect loop: `lash-core/src/runtime/turn_driver/effects.rs:291-311` | A replayed checkpoint is the authority, and "the cells before it re-run on replay (ADR 0103)". Their refilled messages are discarded | covered (V0 path); needs a phase record (L3, FIG-5172) for the production loop | The checkpoint is a column on the turn row (`model.start`, the `tools` advance, `turn.commit`). Restore re-delivers only the machine's pending effect, so no earlier cell runs. A cell re-delivered after its end reads its `Ended` snapshot. V0 observed this: one restore per kill, and zero fresh VM entries after the first snapshot (P4) |
| 2 | Turn driver handlers: `handlers.rs:145` (response hooks), `:369` (prelude by digest), `:674-678` (cell replay key as the observation lane), `:725`, `:902` (journaled peek) | Hook results, the prelude and a peek are served from the journal to a re-run handler. Observation keys come from the effect replay key | needs a phase record (L3) | Each hook result commits with its phase (`model.start` / `turn.prepare`). A peek reads a wait row (L5). Observation keys come from the turn's run and effect id, which the checkpoint holds |
| 3 | `tool_catalog.rs:40,163,262`, `lease.rs:30,53`, `tools.rs:84,114` | The tool surface is replayed per redriven iteration. Lease and command keys are effect replay keys | needs a phase record (L3, L4) | The surface is part of `SyncExecutionEnvironment`'s result, already inside the machine checkpoint (`environment.sync`), so a restore reads it. Command identities are `ToolCallId`s minted at admission (L4) |
| 4 | Turn loop: `turn_loop/accept.rs:159-161,230` | Acceptance is journaled, so a replayed handler re-derives the same admission | needs a phase record (L3s, FIG-5173) | `turn.accept` / `turn.admit` commit the admission. V0 admits from one `session.admit` mail under `turn.admit`, and restore reads the row (`admission_json`) |
| 5 | Turn loop: `turn_loop/prepare.rs:25-35`, `context_pressure.rs:26-28,58,158,310` | Redrive determinism: a replay must hash the same commit content, and compaction serves the recorded text | needs a phase record (L3) | `turn.prepare` commits the prepared view and the compaction base with the phase. A restore decodes them and renders nothing |
| 6 | Turn loop commit: `turn_loop/commit.rs:681,738` | Commit callbacks are served from the journal on replay, "no callback runs again (K10)" | covered for the head (`turn.commit`); needs a phase record (L3) for callbacks | The head CAS, the revision and the terminal are one transaction, published once (P7, observed). Callback results ride `turn.commit` |
| 7 | Logical turn: `logical_turn.rs` follow-ons (FIG-3157) and `turn_loop/follow_on_recovery.rs` | A follow-on physical turn is recovered by redriving the logical run | needs a phase record (L3) | A follow-on is a new admitted turn (`turn.admit`) carrying the withheld work as its input. `RecoverFollowOn` is deleted (row 26 below) |
| 8 | The shift: `shift.rs:1-5`, `shift/admission.rs:18,77,164,223-267` | The root nonce and the admission receipt are journaled, so a retry never re-selects. Redrive intents gate admission | needs a phase record (L3s) | The session mail drain commits selection and sealing in its own transaction. A retry reads the committed rows and never re-runs selection. A redrive intent becomes mail |
| 9 | The shift: `shift/turn_config.rs:12-54` | The resolved config is keyed by the run's replay key, and a redrive replays it | pure once L3s writes it | The admission row carries the resolved config binding. A restore decodes it and reads no live config |
| 10 | The shift: `shift/close.rs:6` | A replay acknowledges the recorded close | needs a phase record (L6b) | Session close is its own phase transaction |
| 11 | `RunCoordinator`: `tool_dispatch/run_coordinator.rs:1-41,170-190` | A/X/D/V records are appended "in program order, so a replay serves them in the order they were recorded". `EffectReplayDivergence` guards the replayed decode | needs a phase record (L4, FIG-5174) | `round::{admit, run_body, settle, fold}`: rows fold to `RunFold` state, and nothing re-runs to regenerate them. V0 shows the `Once` case: an unsettled `x_start` folds to `Interrupted`, committed under `round.outcome` (K1). The drain's rank order becomes phase records (`round.present+model.start`) |
| 12 | `RunCoordinator`: `run_coordinator/parallel.rs:49-302` | Concurrent attempts keep a recorded schedule that a replay walks | needs a phase record (L4) | Each member of a round is admitted at ordinal 1+2i with its outcome at 2+2i (`round::member_ordinal`). The fold reads members by ordinal, never by schedule |
| 13 | `tool_dispatch/production.rs:339,470,620-640,1212,1297` | "Replay consults X's recorded outcome" in place of a live token. The attempt runs inside a replayable step body | covered for `Once` (V0); needs a phase record (L4) for the model tool-call path | `run_body` runs a body only after its admission commits, at most once per admission (P1). The outcome commits under `round.outcome`. The stop watch is the cancel token on the admitted execution |
| 14 | RLM executor: `lash-protocol-rlm/src/native/driver.rs:372-460` (`handle_exec_result`, `projection_rehydration`) | None in itself: it interprets an `ExecResponse` | pure | Its driver state is inside the machine checkpoint. The response it receives is the cell's `Ended` result, read from state |
| 15 | RLM executor: `lash-lashlang-runtime/src/cell_bindings.rs:1-31` | The cell's binding set is journaled so that a redrive links the same surface and its recorded results still apply | pure (V0 path); needs a phase record (L7, FIG-5177) for the worker path | V0 re-links and recompiles the cell from its code, with no recorded result served. The lashlang VM refuses a continuation whose executable differs (`ContinuationError::ExecutableMismatch`, `lashlang/src/runtime/vm/continuation.rs:1517`). The snapshot row carries `executable_identity`. L7 stores the binding set with the snapshot for worker-hosted cells |
| 16 | `worker_execution.rs:224-248` (`Capture`) | The quiet point is kept in memory. On a crash the run restarts fresh, which re-issues effects the lost run already issued | needs a phase record (L7) | `DurableSnapshotStore::{commit_quiet_point, latest}` (V0) commits VM bytes, the ledger and the admissions in one transaction (`cell.snapshot+admit`). L7 swaps `Capture` for it |
| 17 | `replay_run.rs:1-12`, `replay_commands.rs:1-12` | Every command takes the run's next **issue ordinal** (a position), and its journal rows key under it | needs a phase record (L7, L6/L7b for process bodies) | An operation's identity is `OperationId{run, ordinal}`, minted at a quiet point from the snapshot ledger's `next_admission` and committed with the snapshot. A resumed VM re-issues only the operation its continuation is parked on, and it is answered by identity from the fold (`outcomes_to_inject`). V0 counted zero committed ordinals emitted again (P4) |
| 18 | Broker contract: `lash-vm-broker/src/broker.rs:37-41`, `ledger.rs:13-14` | "Ordinals are positions in the invocation's journal". The VM and its counters are "rebuilt only by the substrate replaying the journal" | covered (V0 path); L7 rewrites the contract text for the worker broker | A `Checkpoint{vm, ledger}` commits with the admissions it issued (P6). Restore opens the continuation from the snapshot and folds outcomes from `run_records`, with no journal. The doc text predates FIG-5190, and L7 replaces it along with `Capture` |

## The 34 `RuntimeEffectCommand` variants

These classes refine I0's routing table in `substrate-seams.md` (which method owns each variant) by how each one stops depending on replay.

| # | Variant | Class | Label or lane |
|---|---|---|---|
| 1 | `TransitionPlugins` | needs a phase record | L3s: committed in the drain (`turn.admit`) |
| 2 | `TraceBoundary` | needs a phase record | L3: the trace receipt rides the phase commit |
| 3 | `BeforeLlmCall` | needs a phase record | L3: the hook's result commits with `model.start`. A hook that calls host code is its own admitted execution (verify §3.6) |
| 4 | `LlmCall` | covered | `model.start` pins the attempt (V0 executed). L3 adds `model.done` so a crash after the answer does not pay for a second attempt |
| 5 | `AssistantResponseHooks` | needs a phase record | L3: as `BeforeLlmCall` |
| 6 | `Direct` | needs a phase record | L4: an admitted execution of the calling tool's body |
| 7 | `ToolAttempt` | covered (`Once`, V0 executed); needs a phase record for the model tool-call path | `cell.snapshot+admit` (`x_start`), `round.outcome`; L4 |
| 8 | `PresentToolResult` | needs a phase record | L4: `round.present+model.start` |
| 9 | `Trigger` | needs a phase record | L4: a store-local effect of the settled outcome. V0's `settle` refuses store-local effects until then |
| 10 | `IngestTriggerOccurrence` | needs a phase record | L4: as `Trigger` |
| 11 | `AdmitTriggerDelivery` | needs a phase record | L4: as `Trigger` |
| 12 | `Process` | needs a phase record | L6: store-local start and signal, a wait for await, mail for cancel |
| 13 | `ExecCode` | covered | `cell.snapshot+admit`, `round.outcome`, `cell.snapshot` (V0 executed, P2 to P6) |
| 14 | `AcceptTurnInput` | needs a phase record | L3s: `turn.accept` |
| 15 | `ObserveDrainMark` | deleted | a drain is a release at a committed phase (L11) |
| 16 | `PluginCallbacks` | needs a phase record | L3s |
| 17 | `RecoverFollowOn` | deleted | restore reads the turn row (`restore_turn`, V0) |
| 18 | `RestoreRunMaterial` | deleted | the fold reads committed records (`RunFold::material`, V0) |
| 19 | `AdmitShift` | needs a phase record | L3s; V0 covers the admit shape (`turn.admit`) |
| 20 | `DrawRunStart` | needs a phase record | L3s |
| 21 | `ResolveTurnConfig` | pure once recorded | L3s writes it into the admission row |
| 22 | `RecordCompactionBase` | needs a phase record | L3: `turn.prepare` |
| 23 | `RenderCompactionPrompt` | needs a phase record | L3: `turn.prepare` |
| 24 | `ResolveConfigTransaction` | needs a phase record | L3: under the session epoch |
| 25 | `ReadSessionCommandRun` | needs a phase record | L3 |
| 26 | `CloseRunScope` | needs a phase record | L3: `turn.commit` ends the run's scope |
| 27 | `BeginSessionClose` | needs a phase record | L6b |
| 28 | `Checkpoint` | covered | the checkpoint column on the turn row, written by `model.start`, the `tools` advance and `turn.commit` (V0 executed; one restore per kill, P4) |
| 29 | `SyncExecutionEnvironment` | pure (V0); needs a phase record (L3) for plugin-driven syncs | the sync result lands in the checkpoint's `environment`; `turn.prepare` |
| 30 | `LoadExecutionEnv` | needs a phase record | L6: admitted execution |
| 31 | `Sleep` | needs a phase record | L5: a pinned timer row |
| 32 | `AwaitEvent` | needs a phase record | L5: a pinned wait row raced against cancel mail |
| 33 | `PeekAwaitEvent` | pure | a read of a wait row, recorded nowhere |
| 34 | `LanguageRuntimeValue` | deleted | values live in the VM heap snapshot (`cell.snapshot`) |

## verify.md §3: where a design drifts back toward replay

| Item | Risk | Class | Resolution |
|---|---|---|---|
| 1 | opus §3.7: a host `ProcessEngine` cold path re-invokes `run` from the top and answers each `step(id)` from its row | needs a phase record (L6, FIG-5175); not adopted | The engine's state is a snapshot under `ExecKey::Process`, written inline in `snapshot_ref` by `process.advance` (V0's answer to L6, message 86152). An engine advances from its committed state and is never re-invoked over recorded steps |
| 2 | opus §3.2: the "re-entry rule" re-executes kernel code from its last committed phase, with outcomes served from `lash_step_outcomes` by key | covered by not adopting it | V0 has no step-outcome table. The activation restores the machine from the checkpoint, and the machine re-delivers its one pending effect. That effect is either new work under a new pinned attempt (the model) or is read from state (a cell's snapshot). Lanes L3/L3s add the remaining kernel phases as phase records, not re-entry |
| 3 | opus R-4 fallback: snapshot every N blocks and re-execute between snapshots | pure (V0); the fallback is not adopted | V0 snapshots at every quiet point that issues an operation and at the cell's end. Code between quiet points issues no operation, so recomputing it serves nothing recorded: a crash before the first snapshot re-enters the program from instruction 0 with nothing to look up. P4 bounds fresh entries at 1 plus uncommitted snapshots, and the K1/K2 runs saw exactly 1 |
| 4 | `lash_run_records` with an ordinal per row | covered | `round::fold` folds the rows to `RunFold` state, and resume never re-runs a producer. The P4 counter for re-emitted committed ordinals stayed at 0. That probe has no production call path: the absence is structural, since nothing on the resume path emits a ledger ordinal |
| 5 | astra: VM and broker ordinals as journal positions (`broker.rs:37-41`, `ledger.rs:13-14`) | covered (V0 path); L7 rewrites the worker broker | See row 18: an operation's identity is the admitted `OperationId` in the snapshot's ledger, not a journal position |
| 6 | astra: preparation and plugin hooks re-run against pinned outputs | needs a phase record (L3) | Each hook that can call host code is its own admitted execution with a phase commit after it. A deterministic hook is pure recomputation at its phase. V0's path runs no hooks |

## Accepted costs, not replay

- **`ack-hidden` at `cell.snapshot+admit`.** The snapshot and `x_start` committed, but the node never learned it. The next pass folds the `Once` operation to `Interrupted` without its body ever running. A `Once` operation never runs twice, so after an unknown commit it may not run at all. The cell receives `Interrupted`.
- **A crash after the model answered and before the next commit.** The next pass calls the model again under attempt 2. This is a second attempt of an at-least-once effect, not an answer served to re-run code. L3's `model.done` removes the cost.
