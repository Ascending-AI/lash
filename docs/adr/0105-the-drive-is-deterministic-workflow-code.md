# 0105: The drive replays recorded decisions through the controller

## Status

Accepted.

## Context

A durable drive must resume without choosing different effects from live
clocks, caches, cancellation tokens or observation arrival order. The turn
machine consumes recorded inputs; the controller records the operations whose
answers decide work. This is the execution contract of ADR 0104 §2.
Durability is the goal. Deterministic scheduling of the test harness is not.

## Decision

### 1. The drive uses the recorded controller

A drive issues effects through its scoped `RuntimeEffectController`. Restate
records the canonical envelope and outcome under the effect's replay key,
compares the envelope on replay, and serves the recorded outcome. Code cells
reconstruct interpreter state by re-execution under ADR 0103.

Drive decisions derive from recorded outcomes, immutable admitted inputs and
pure functions of them. Observation is addressed by replay identity and
ordinal; it cannot decide a commit. A live storage check can refuse stale or
inconsistent execution, but cannot replace recorded work with newly selected
work. Restate records durable timers, keyed waits and cancellation races.
Opaque tool bodies execute inside recorded attempts under ADR 0116; they are
not replayable orchestration drivers.

Evidence: `crates/lash-restate/src/controller/journaled_effect.rs:314`,
`crates/lash-restate/src/controller/execution.rs:122`,
`crates/lash-core/src/runtime/observation_publisher.rs:1`,
`crates/lash-core/src/runtime/turn_boundary/recorded_assembly.rs:1`,
`crates/lash-core-execution/src/engine/testing/controller.rs:40`.

### 2. Unfenced admission, separate from fenced execution

`AdmitDrive` records the selected root and admission identity. Before execution,
`DrawRootStart` records a random nonce in the root's journal, and
`SealDriveAdmission` raises the drive epoch by store compare-and-set. The same
admission and nonce replay the stored fence. A different nonce for the same
sealed admission is `ExecutionLost`, reported as `SubstrateLost`, and runs no
root work. A child inherits authority rather than minting an epoch. L-S8 is
the missing-history law.

`AdmitRoot`, keyed by the root, binds its rows and records the base
`SessionHeadRef`, turn index and executable generation in `RootAdmission`.
Replay reconstructs the admitted state rather than selecting from the live
head. A base the store cannot retain refuses `TurnBaseNotRetained` and parks.
An unfinished root keeps ownership of its admitted head until terminal evidence.

`InspectAdmittedHead` records one of these verdicts:

- `Ready` reconstructs the admission's base, including when the root already
  has a committed turn.
- `Advanced` reconstructs the head the unfinished root's fenced commits
  publish, including its own frame changes.
- `Overtaken` means another writer advances past the base. The root ends with
  `StoreCommitSuperseded` before turn effects.
- `Diverged` means a lower revision or the same revision with inconsistent
  leaf or checkpoint. The root parks.

Replay honors the recorded verdict at every journal position. A head that
moves after a recorded `Ready` is met by the fenced commit, rather than a live
re-check that changes the command stream.

A live read outside the recorded steps never decides which steps a root
journals: not the resident-session refresh before `AdmitRoot`, not the
engine's opening of the session. A sealed root whose drive holds no current
head, because the engine cannot open its session (its close or tombstone
committed) or because the refresh fails, runs headless. It still issues its
recorded steps under the envelopes a root with a head issues: `AdmitRoot` and
`InspectAdmittedHead` for an input- or queued-headed root, and the
`session-command-run:{n}` reads for a command root. A command root also reads
on headless once its session retires under a run it read, because every
command but a compaction settles and commits off the journal. The bodies of
those steps admit, inspect and read nothing. A deleted session is the step's
recorded outcome: the catalog's tombstone, read inside the step, or a session
the engine cannot open. The root then ends with the typed `SessionDeleted`
refusal of [ADR 0049](0049-session-ids-are-used-once.md) where its journal
holds nothing more. Any other refresh fault is the attempt's and is recorded
nowhere. A journal holding work past those steps (the turn after
`InspectAdmittedHead`, a compaction's apply) cannot be retraced without the
session's head. That attempt ends as a live fault that journals nothing, and
the engine's park reconcile releases a root whose session stays deleted as
`TargetGone`. A follow-on recovery root records no step between its seal and
its turn, so it still decides live (FIG-4361).

A sealed execution carries `DriveFence`. Head-changing writes and ingress
settlement check that fence in their transaction. Replay envelopes do not hash
the fence into effect identity, L-S12. A refusal no retry can change ends its
root through `RootTerminalCause::Refused` before the engine records the outcome.
That idempotent end presents the run's fence; a stale owner leaves the
root to its current owner. A park, unknown commit outcome or retryable live
fault ends no root. A later epoch adopts existing root terminal evidence,
L-S6, rather than replacing it with a new commit.

Evidence: `crates/lash-core/src/runtime/drive/admission.rs:94`,
`crates/lash-core/src/runtime/root_start.rs:1`,
`crates/lash-core-store/src/store/drive_fence.rs:185`,
`crates/lash-core/src/runtime/drive/root.rs:103`,
`crates/lash-core/src/runtime/drive/root.rs:346`,
`crates/lash-core/src/runtime/drive/root.rs:800`,
`crates/lash-core/src/runtime/drive/root.rs:557`,
`crates/lash-core/src/runtime/drive/root.rs:605`,
`crates/lash-core/src/runtime/drive.rs:894`,
`crates/lash-core/src/runtime/drive.rs:930`.

### 3. Cancel races and losing work

The controller records the winner of a durable wait and its cancel gate.
Replay uses that winner. An `AfterStep` request remains deferred while the
step runs, and an `Immediate` escalation can stop the guarded wait. A recorded
step keeps a live cancel watch inside its body; what the body returns is the
recorded outcome. Replay does not re-run the watch.

A watch failure is not cancellation. The shared retry ladder has eight
attempts, starting at 25 ms and doubling to a 1 s cap. A step that can leave
a derivation unrecorded ends the attempt with `TransientCancelWatch` when the
watch gives up. A tool attempt whose engine records every returned outcome
runs to its own end instead; the drive observes cancellation at its next
recorded peek.

The drive advances its honored cancellation fact only through recorded peeks
and outcomes. A host-local stop is forwarded as a durable gate request.
Execution-side tokens stop bodies; they do not select drive work.

A code cell observes cancellation at instruction-count checkpoints. Its first
gap is 2^20 instructions; gaps double to a cap of 2^28. The accounting and
cell grammar belong to its executable generation. Process waits and body
checkpoints observe the process's durable cancellation fact. The process
handler journals registry steps for admission, resume, completion, boundary,
handover, cancel forwarding and handover retirement. A retryable store fault
is the attempt's fault rather than a durable decision.

Evidence: `crates/lash-restate/src/controller/context.rs:1`,
`crates/lash-core-execution/src/runtime/turn_control/local_stop.rs:235`,
`crates/lash-core-execution/src/runtime/turn_control/local_stop.rs:350`,
`crates/lash-core-execution/src/runtime/turn_control/local_stop.rs:369`,
`crates/lashlang/src/runtime/mod.rs:185`,
`crates/lash-restate/src/process/workflow.rs:1`.

### 4. Group operations are complete

The controller opens durable groups, serves ranked settlements, reads a rank
without advancing its cursor, commits child final results under the cancel
fence, waits for protected drain and closes under the loser policy.
`EffectGroupHandle` owns the settlement cursor. ADR 0099 defines the child
attempt, committed and seated boundaries.

A wait child races its guarded timer or keyed wait against the child's durable
cancel wait in the journal. A tool child's driver peeks before attempts, and
the attempt body watches the decided cancel. The attempt's cancelled outcome
is recorded. A lost watch does not fabricate cancellation. Settlement,
retirement and cancel admission retain their group fences across replay.

Evidence: `crates/lash-core-execution/src/runtime/effect/executor/control.rs:369`,
`crates/lash-restate/src/effect_group/child_cancel.rs:1`,
`crates/lash-restate/src/controller/context/child_cancel.rs:1`,
`crates/lash-restate/src/effect_group/dispatch.rs:368`,
`crates/lash-core-execution/src/runtime/turn_control/local_stop.rs:300`.

### 5. Keyed promises

A resolution persists before a waiter exists. The first writer wins.
`ResolveOutcome` distinguishes `Accepted`, `AlreadyResolved { terminal }` and
`UnknownOrRevoked`; a duplicate returns the retained terminal and cannot
overwrite it. Revoking a session tombstones its wait index and cancels its
waits; later resolves return `UnknownOrRevoked`.
An acknowledged resolution is durable. The engine's wait identities remain
private behind neutral keys under ADRs 0003 and 0012.

Evidence: `crates/lash-restate/src/durable_wait.rs:1057`,
`crates/lash-restate/src/durable_wait.rs:1414`,
`crates/lash-restate/src/durable_wait/root_retirement.rs:1`,
`crates/lash-core-effect/src/retirement.rs:200`.

### 6. Protocol drivers and hooks use recorded inputs

`ProtocolDriverHandle` and `ContextProjector` are synchronous decision APIs
over recorded inputs. Interior mutability that changes replay decisions
violates their contract. Provider and tool I/O goes through recorded effects.
A plugin's live mutable memory is not the authority for a replayed decision.

Hooks receive read views and return decisions; core owns the resulting writes.
The context-pressure hook runs before Prompt View transforms with the
committed view, previous prompt usage and context window. `Record` nodes join
the turn's append draft. `OpenFrame` performs its own fenced, idempotent commit
before the turn's model call. The frame key and operation identity derive
from the turn and hook. Replay uses the admitted base and recorded summarizer
completion, and checks the frame commit's receipt. A later turn failure leaves
that committed frame in place. The write is a store operation under §9, not
an extra controller command variant.

Evidence: `crates/lash-sansio/src/sansio/turn_protocol.rs:699`,
`crates/lash-sansio/src/sansio/turn_protocol.rs:779`,
`crates/lash-core/src/runtime/turn_loop/context_pressure.rs:1`,
`crates/lash-core/src/runtime/turn_loop/context_pressure.rs:114`.

### 7. Continue-as-new and version decisions

A session drive runs at most `MAX_ROOTS_PER_DRIVE`, 64 roots, per invocation.
At a root boundary the session driver sends a continuation with an idempotency
identity derived from the drive request. It transfers no live turn machine or
group cursor. Process execution uses bounded segments and durable handovers.

`DriveRequest.build_generation` names the build generation. Recorded routes
and generation sentinels bind journal replay under ADR 0106 §1. A process's
next segment can enter the latest compatible build; replay does not patch the
old segment's journal in place. Durable formats follow the current freeze
and compatibility rules of ADR 0106.

Evidence: `crates/lash-core-execution/src/engine/drive.rs:1`,
`crates/lash-restate/src/session_driver.rs:1155`,
`crates/lash-restate/src/process/workflow.rs:1`,
`crates/lash-core-execution/src/engine/contracts.rs:37`.

### 8. Send and !Send

`RuntimeEffectController` is `Send + Sync` through its resolver contract and
returns `Send` async futures. The local replay-check harness accepts a
`!Send` drive future and polls it on one thread. Production Restate handlers
use the production controller. The local harness's execution model supplies
no shipping engine or scheduling guarantee.

Evidence: `crates/lash-core-execution/src/runtime/effect/executor/control.rs:369`,
`crates/lash-core-execution/src/engine/testing/check.rs:1`.

### 9. Commit, park and settlement use fenced store writes

Core assembles the commit from recorded results and calls
`commit_runtime_state_verified`. The store checks head, fence, operation
identity and existing receipt, then writes the head, root terminal evidence,
usage and ingress settlement together. Lost replies are checked against the
stored commit. The drive can safely repeat the idempotent write.

A classified root park is an idempotent store write that cannot replace an
existing terminal. Reconciliation can write the same park. A non-retryable
root refusal writes its terminal before returning its engine outcome, as in
§2. Commit, park and ingress settlement have no separate journaled command
variants. External operations still need stable idempotency identities; a
store fence cannot retract a request already sent.

Evidence: `crates/lash-core/src/runtime/turn_boundary.rs:1`,
`crates/lash-core/src/runtime/turn_loop/commit.rs:1`,
`crates/lash-core/src/runtime/drive/park.rs:1`,
`crates/lash-core/src/runtime/drive.rs:930`,
`crates/lash-core-store/src/store/runtime_commit.rs:1`.

### 10. Commands and executors

A serialized `RuntimeEffectEnvelope` produces a `RuntimeEffectOutcome` or a
typed controller error. The runtime executor supplies the recorded body; live
services do not become part of command bytes. Restate validates the canonical
envelope before returning a recorded result. An envelope mismatch is
`EffectReplayDivergence` and parks.

Environment sync records prompt and catalog definitions, and the drive
installs what it returns. A tool child loads its execution environment through
its own `LoadExecutionEnv` step. Deterministic refusals remain recorded answers.
A live store or session fault in an uncommitted derivation is retry authority:
the engine ends the attempt without recording that fault as the step's answer.
The same rule applies to sync and assistant-response hook derivations.
Recorded tool definitions and drift handling follow ADR 0103.

Evidence: `crates/lash-core-execution/src/runtime/effect/envelope.rs:1`,
`crates/lash-restate/src/controller/journaled_effect.rs:314`,
`crates/lash-core-execution/src/runtime/effect/tool_child_driver.rs:1`,
`crates/lash-core-execution/src/runtime/effect/executor.rs:1`.

### 11. Validation and laws

Replay evidence includes a cold replay, a separate worker without live caches,
and perturbations of recorded-outcome delivery. The local `DeterminismCheck`
compares command transcripts and commit bytes across these modes; its name
does not impose deterministic scheduling on production or lash-sim.

L-S3 and L-S4 require one durable epoch transition per admission, regardless
of seal-body retries. L-S6 adopts the terminal of an already-committed root.
L-S8 refuses fresh execution after retained start history is lost. L-S12
keeps drive fencing out of replay-envelope identity.

Storage laws run on SQLite file, SQLite memory and PostgreSQL. Execution
hosts are the in-process Restate server double, live Restate and lash-sim's
in-process effect host. The server double supports crashes and always-replay
through its backend constructor. Restate cancellation and process crash
matrices exercise the production handlers. Upgrade proofs use synthetic-next.

Evidence: `crates/lash-core-execution/src/engine/testing/check.rs:25`,
`crates/lash-conformance/src/conformance/drive_admission.rs:1`,
`crates/lash-conformance/src/conformance/root_start_marker.rs:1`,
`crates/lash-restate/src/tests/drive_laws_on_the_double.rs:1`,
`crates/lash-restate-test/tests/process_crash_replay.rs:1`,
`crates/lash-upgrade-harness/tests/phase_a/main.rs:1`.

### 12. Journal generations and the version freeze

Recorded effects carry `effect_journal_version`. A foreign or absent stamp
decodes as a retained value, then refuses through the controller as replay
divergence rather than looping on a deserialization failure. The first session
or turn step also carries its build-generation sentinel. Process and group
journals have their own registered drain surfaces.

The pre-1.0 freeze changes shapes in place, without version bumps or upcasters.
The existence of a stamp does not promise compatibility across those edits.
Release compatibility, migration and drain are governed by ADR 0106 and
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).

Evidence: `crates/lash-restate/src/controller/effect_journal.rs:182`,
`crates/lash-restate/src/controller/effect_journal.rs:226`,
`crates/lash-restate/src/sentinel.rs:89`,
`scripts/versioned-surfaces.toml:1`.

## Rejected alternatives

A poll interface for the entire drive adds suspension machinery around a VM
that already awaits controller effects inline. Separate `Send` and `!Send`
controller traits duplicate the contract. An epoch check alone cannot fence
an external request already sent; stable idempotency covers that boundary.
A recorded step cannot select a live body away outside the body and still
replay its outcome. Cancellation therefore belongs to the recorded wait or
step body.

## Consequences

Durable replay uses recorded decisions and repeats fenced, idempotent writes.
Live observation and cancellation transport do not replace recorded facts.
Controller adapters own journal validation and durable wait races, while core
owns commit and terminal evidence. The contract applies to drivers and engines;
opaque tool bodies are recorded execution. Changes under the pre-1.0 freeze
carry no cross-build compatibility guarantee.
