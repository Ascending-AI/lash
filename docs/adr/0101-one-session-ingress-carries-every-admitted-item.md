# 0101: One session ingress carries every admitted item

## Status

Accepted.

## Context

Host input, generic queued work, and session commands require durable admission,
comparable ordering, idempotent settlement, and explicit cancellation outcomes.
A frame switch's follow-on must survive any crash between the switch and the
turn that runs it.

## Decision

A session has one logical durable ingress composed from its admission tables.
The session actor's owner executes admitted turns
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §3). Callers submit durable data and observe the
result; they do not own a turn's continuation.

### 1. The model

`pending_turn_inputs` holds host input. Each `queued_work_batches` row holds
one generic work item or session command in `payload_json`; its `work_kind` CHECK
agrees with the payload.
`session_ingress_sequence` allocates one per-session `enqueue_seq` across both
admission families. The counter is allocated inside the producer transaction.
The command lane is selected by kind; input and generic work form the turn lane.

A selected row records `admitted_run` and `admitted_by`. Run and checkpoint
admissions are fenced writes under the session actor's epoch. The producer
transaction that inserts a row wakes the session actor (ADR 0132 §12), so no
delivery is owed after admission. `IngressSettlement`
names the run and its completed inputs, completed batches, released rows, and
dropped rows. There is no durable claim token or queued-run ledger.

Selection is a recorded admission. The store records the selected result on
`session_runs` in the same transaction as binding rows. Resume reads that
result instead of choosing again. A checkpoint names its own recorded
admission. A run's fenced commit or terminal write settles or releases its rows.
The session has at most one admitted unfinished run. A terminal run's owed
scope close holds no admission and does not block the next run.

Evidence: `crates/lash-core-store/src/store/admission_plan.rs:1`, `:31`, `:67`,
`crates/lash-core-store/src/store/run.rs`,
`crates/lash-sqlite-store/src/persistence/admission.rs`,
`crates/lash-postgres-store/src/postgres/runtime_persistence/admission.rs`, and
`crates/lash-store-sql/src/session_ingress.rs`.

### 2. What stays separate, and why

Product routing and schedules belong to the host under
[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md). A host sends a notice only after its source fact commits,
using `send().id(TurnId)` to make retries idempotent. Its event ledger is not
another turn ingress. Internal actor wakes notify runtime work; they carry no
product event or routing decision. Turn cancellation follows
[ADR 0039](0039-turn-cancellation-is-a-first-party-work-driver-primitive.md).
The host's `QueuedDrainPolicy` bounds generic work composition.

### 3. A frame switch mails its follow-on with its commit

A turn that ends with `AgentFrameSwitch { task }` answers its run. Its
`turn.commit` publishes the head's new frame pointer with the turn's terminal
and, in the same session transaction, mails the session its follow-on: one
next-turn input holding the task, written by the owner's
`SessionMailWrite::Enqueue` exactly as a producer's acceptance writes it,
with the session's wake. The input's source key and id name the follow-on's
run, `frame-task:<frame key>`, so a retried commit mails the same input and a
frame's task runs once. No crash separates the switch from its follow-on:
they are one commit.

The follow-on is ordinary session mail. The session's next drain admits it
as a turn, which runs the task on the new frame under the session's config.
It takes the session's next ingress position: work queued before the switch
committed runs first, and commands apply at the boundary between the two
runs, as at any run's end (§4). A closing or deleted session takes no mail,
so its close settles the switch's work with the rest of its own. A host
cancels the follow-on as it cancels any queued input or running turn.

A chain is bounded by the deployment's
`ExecutionBudgetsConfig::agent_frame_switch_limit` (nonzero; 16 in the
explicitly selected `ExecutionBudgetsConfig::recommended()` preset).
Fresh host input and host notices start at depth zero. The switch's mailed
`TurnInput::agent_frame_switches` is its admitted depth plus one, persisted
with the task and included in its submission digest. A claimed or resumed
turn reads that immutable depth from its recorded admission. The follow-on
at the configured bound commits `TurnStop::AgentFrameSwitchLimit` and emits
`TurnFailureCode::AgentFrameSwitchLimit`, without calling the model or mailing
another follow-on. Its ordinary turn commit settles the input and closes the
run's scope. A chain below the bound completes normally, and later host input
starts a fresh chain regardless of the session's frame history.

A frame-task input runs alone: it neither takes later host input into its run
nor joins an earlier input's batch. Thus the chain's depth and terminal do
not apply to unrelated input. Its ingress position still obeys ordinary FIFO.

The `send()` of the switching input answers with the switch
(`TurnStatus::Answered`). The follow-on's run answers under its own id,
which a host attaches to by that id.

Definition carry prepares successor-frame edges before the head CAS and
retains the complete closure after a committed switch. Guarded cleanup uses
frame retention and execution end evidence under
[ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md).

Evidence: `crates/lash-core/src/runtime/durable/phases.rs` (`turn.commit`),
`crates/lash-core/src/runtime/durable/session_mail.rs` (`follow_on_mail`,
`frame_task_run`), `crates/lash-sqlite-store/src/durable/session_mail.rs`,
`crates/lash-postgres-store/src/postgres/durable/session_mail.rs`, and the law
`crates/lash-durable-test/tests/frame_switch_crash_proof.rs`.

### 4. Session commands are a lane applied at turn boundaries

Commands apply at idle or after a logical run finishes, ahead of fresh
turn-lane runs. They do not apply mid-turn, at checkpoints, or between the
physical turns of one logical run. An unfinished run retains precedence; a
parked run whose resume is unsettled holds the command
lane as it holds inputs, with the typed, retryable `SessionRedriveUnsettled`,
because the unfinished run owns the head. A checkpoint does not treat an open
command as a barrier.

Commands are not admitted turn rows. The session actor's owner selects the
leading command run and settles it in the applying commit. The owner's fenced
read of the run is the commands' admission, and a withdrawal reaches a command
only before that read. Every command applies alone, with its
own revision check, recorded resolution and receipt; adjacent commands never
merge into one admission. A config change is one such command: a typed
`ConfigTransaction` submitted under `ConfigWrite { id, expected_revision }`
through `SessionConfigAdmin`
([ADR 0126](0126-session-config-changes-are-typed-owner-commands.md)), whose
recorded resolution — including the route check for a changed core route —
settles `Applied`, `Stale` or `Refused` as §12 records, and a refusal
publishes no config and reaches no queued turn. Each turn retains its recorded config snapshot through checkpoints
and follow-ons. A command can therefore change the config used by an input
queued before it. A host requiring an earlier config waits for the input's
answer before submitting the transaction.

`CompactContext { instructions }` is a command. It applies under the session
actor's epoch fence, records its summary, opens the frame and records usage
with settlement in one commit. Its typed outcome is `Opened`,
`NothingToCompact`, or `Failed`. A resumed command whose settlement committed
adopts the published head and cannot open another frame. A storeless runtime serializes direct compaction
through its mutable runtime access.

Every host head write from outside a turn is a command applied against the
boundary's resident head: an append (`AppendSessionNodes`), a plugin command
(`RunPluginCommand`) or task (`RunPluginTask`), and a durable frame open
(`OpenAgentFrame`). A store-backed runtime refuses the direct calls with
`SessionCommandRequired`; only a storeless runtime applies them directly. Each
applies alone and settles in the one commit that makes its head write. An
append lands its nodes, or settles `StaleBranch` when its required ancestor
left the active path. A plugin's code runs only after admission; its services
join the command as in-turn services join a turn, so its graph appends, usage,
runtime events, plugin state, and queued turns ride the command's commit. A
task is a tool-bearing operation (K8, binding Q2, FIG-4888): at the head of
the lane it is admitted as its own run, `AdmittedWork::Operation`, named by
the operation (`shift-operation:<batch>`), so a resumed owner admits the same
run. The command run applies the commands ahead of a task and stops at it. The
operation run records the task's effects as phase rows under the command's own
session-operation scope and owns them, and its apply returns only once the
task returned and every effect it issued has settled; a request after the
task returned is refused. A frame
open opens its frame and restarts the live interpreter from the seed. A command
that cannot apply, including one whose commit exceeds the commit budget,
settles with its typed refusal, so the lane never waits on it. The one command
that cannot settle is one whose bare settlement exceeds the budget, on a head a
host lowered the budget below (ADR 0058): its owner stops at the typed
refusal, and the command settles once the host raises the budget.

Head-writing host commands return their typed `SessionCommandOutcome` through
`SessionCommandSettlement::Applied { receipt, outcome }`. Config transactions
carry their typed applied, stale or refused result as described in §12;
a catalog refresh answers `Durable`. A host submits with a stable idempotency
key and gets a durable receipt (`SessionCommandAdmin::submit`). A resubmission
with equal key and content returns the first receipt while the command is open
or retained as a tombstone. `settle` answers the recorded result, `Cancelled`,
or `Pending` with the receipt when the owner has not applied the command by
the deadline, and a host reattaches by the receipt. The convenience calls
(`append_messages`, `append_session_nodes`, `open_agent_frame`, the plugin
operations, `compact_context`) submit and await. Dropping an await does not
withdraw the command; `withdraw` does, transactionally, and answers
`AlreadyAdmitted` once an owner read it. Plugin tasks expose their own
operation-run handle: `PluginOperations::start_task` returns a `RunHandle`,
and `RunHandle::cancel` uses `CancelBuilder` to request that logical owner's
cancellation. Cancellation of the task withdraws its command row. The actor
watches that row while the task runs and fires its cancellation token when
withdrawal is observed. The applying commit requires the row still to be
open, so a withdrawn task publishes none of its head changes; a settled task
keeps its recorded outcome. This is distinct from withdrawing an ordinary
command before its admission. The owner executes and settles the task; the
host only submits, follows or cancels it. The runtime
writer is never held while a settlement is awaited. A command run, once it drained the lane, writes its
`RunTerminalCause::CommandsApplied` terminal and closes its scope in that
transaction, so its phase rows are retired like a turn run's; an operation
run ends the same way once its task settled. A resumed operation run past its
settling commit runs nothing and adopts the published head.

Evidence: `crates/lash-core/src/runtime/session_api.rs`,
`crates/lash-core/src/runtime/compact_context.rs`,
`crates/lash-core/src/runtime/host_commands.rs`,
`crates/lash-core/src/runtime/host_commands/task_cancel.rs`,
`crates/lash-core/src/runtime/durable/session.rs` (`SessionActivation`),
`crates/lash-core/src/runtime/host_commands.rs`
(`apply_plugin_operation_command`),
`crates/lash/src/admin/host_commands.rs`, and
`crates/lash-core-store/src/store/mod.rs`.

### 5. Ordering and composition

Within the turn lane, the shared `enqueue_seq` is the order; there is no
producer priority. Commands are a separate lane and do not establish a turn-lane
stop. Clock values can bound age; they cannot decide order. Lash implements no
authentication or security policy.

Evidence: `crates/lash-core-store/src/store/admission_plan.rs`,
`crates/lash-core/src/runtime/durable/session.rs`, and
`crates/lash-core-store/src/store/queued_work.rs`.

#### 5.1 Turn addressing is immutable intent

An input's submitted delivery is written once and never rewritten.
`Turn { turn_id: T, min_boundary }` is eligible only at T's admitted checkpoints
while T runs. Once T is not running it is eligible as next-turn input at its
existing sequence position, by rule rather than stored delivery mutation: an
open row is next-turn input when its delivery is next-turn, or when it
addresses a turn other than the running one, and with no turn running every
open row is. A run's release hands an accepted row back open in the state its
delivery names, and deferral at a cancel writes nothing.

Admission accepts the address only if T is this session's running turn or has
ended. T is running when it is a physical turn of the unfinished run or of a
member that run's admission composed. T has ended when its final commit or its run's terminal is recorded. An unknown
turn, another session's running turn included, is refused with
`StoreError::IngressTurnAddressUnknown` (`TurnAddressUnknown` to the runtime)
before any row or sequence number is allocated. A resubmission of an admitted
row is answered by its digest before the address is consulted.

Evidence: `crates/lash-core-store/src/turn_input_vocabulary.rs:292`,
`crates/lash-core-store/src/store_backend_support/queued_work_admission.rs:77`,
`crates/lash-store-sql/src/turn_ingress/pending_inputs.rs`
(`earliest_next_turn_candidate_seq`, `release_run`), and
`crates/lash-conformance/src/conformance/runtime_persistence/ingress_integrity.rs`.

#### 5.2 Composition

Idle admission compares the next input with the earliest queued turn work.
A composition of either admission family stops at the other family's earliest
open turn row. It never skips that stop to take later work. A checkpoint can
select input addressed to its running turn. Its unaddressed prefix stops at a
delivery mismatch and at kind, total, or host policy bounds. An idle
composition of next-turn input stops at a run-spec change and at the host's
input bound. The host's drain policy chooses how much eligible work to take,
of either family: each candidate names its `QueuedDrainFamily`. `authority`
and `merge_key` are per-item data for policy and traces, not equality gates
for composition: a host that keeps principals apart does so in its
`QueuedDrainPolicy`, which sees each candidate's authority and merge key; host
input carries neither. The default policy takes one row at a time, so each
next-turn input is its own run and a cancel of one never reaches another.

Evidence: `crates/lash-core-store/src/store/queued_work.rs`
(`select_leading_session_command`),
`crates/lash-core-store/src/store/admission_plan.rs`
(`plan_next_turn_input_admission`),
`crates/lash-core-store/src/queued_drain_policy.rs`, and
`crates/lash-core/src/runtime/durable/session.rs` (`admit_turn`).

One run answers every input it admits, at idle or at its checkpoints. Each
input retains its own application evidence even when inputs share a run's
answer.

### 6. Render order

Admitted input and generic work keep their recorded presentation order.
A host process notice is ordinary sent input and carries the options its host
submitted. Product formatting belongs to that host under ADR 0137.

### 7. Admission, deferral and resume

An interrupted selection resumes from its run's recorded admission. `Defer`
releases the row binding at its own sequence position; subsequent admission
recomposes open rows. A stale epoch fails the fence: admission and settlement
write nothing, and the stale owner drops the actor. Settlement checks that every named row belongs to the run.
Completed input carries application evidence. Run terminal writes release
remaining bindings, so terminal runs cannot retain admitted rows.

Evidence: `crates/lash-core-store/src/store/admission_plan.rs:67`, `:197`,
`crates/lash-core-store/src/store/run.rs`, and
`crates/lash-sqlite-store/src/persistence/ingress_settlement.rs`.

### 8. Dedup, digest and tombstones

Admission records an immutable submission digest. Equal source key and digest
returns the existing item, open or terminal. A changed digest returns a typed
content conflict for every kind, without silently adopting different content:
`PendingTurnInputSourceKeyConflict` for input and
`QueuedWorkSourceKeyConflict` for queued work. A command's digest covers the
command, its delivery policy, its
authority and its merge key. A config transaction command's digest covers
its id, the revision it was written against and its ordered commands, and
the record holds nothing of the build that took it, so a resubmission from
another build is the same request; the lane refuses a changed one as
`ConfigSubmitError::ChangedContent`. System source-key namespaces belong to
their item kinds.
Admission reserves `command:` for session commands. Input and queued-work
producers enforce this before deduplication or
allocation, and a mixed input request is refused atomically. Plugin commands
queue their inputs as one request, with generated keys under `input:command:`;
an admission refusal retains its typed cause through command settlement to the host.

Terminal items stay in place as tombstones. Tombstones preserve kind, source
key, sequence, submitted delivery, digest, terminal cause, and terminal time,
with no admission binding. A queued-work tombstone records `terminal_cause`
and `terminal_at_ms`; an input's terminal state owns its cause and terminal time, decoded together
from `state`, `ingress_json` and `terminal_at_ms`. Cancelled items cannot reopen on retry. Terminal causes
distinguish delivered input or work, applied command, stale config revision,
and cancellation.
Open-row selection excludes tombstones. Host vacuum removes queued-work
tombstones and withdrawn input; an input a run took keeps its tombstone
beside that run's terminal evidence until session deletion.

Evidence: `crates/lash-core-store/src/store/ingress_terminal.rs`,
`crates/lash-core-store/src/store_backend_support/queued_work_admission.rs:29`,
`crates/lash-store-sql/src/turn_ingress/queued_batches.rs`
(`settle_admitted`, `settle_command`, `withdraw_open`, `delete_tombstones`), and
`crates/lash-conformance/src/conformance/runtime_persistence/ingress_integrity.rs`.

### 9. Host delivery identity

A product delivery uses a stable send id or host start key. The host records its result
before pruning can remove start-key evidence. A cursor is acknowledged only
after its page is recorded or its keyed deliveries complete. ADR 0137 owns
this contract; queue ordering remains the session's ingress sequence.

### 10. Cancel by author

A turn cancel applies its accepted undelivered-input policy to host input
addressed to that turn. `Defer` is the default and writes nothing; `Drop`
records cancellation. Once the turn's run has terminal evidence, the run's
terminal write has already applied the disposition, and a later teardown of
that turn reaches no input. Other held input is released.
`TurnCancelInputOutcome` records the affected
inputs and their disposition. Host withdrawal may remove undelivered generic
queued work; host event records remain under the host's retention policy.

Dropping an observation handle cancels nothing.

Evidence: `crates/lash-core-store/src/turn_control_vocabulary.rs:54`, `:100`,
`crates/lash-core-store/src/store/admission_plan.rs:376`, and
`crates/lash-sqlite-store/src/persistence/ingress_settlement.rs:153`.

### 11. Recompose and render admitted work

Work retained across a physical-turn follow-on carries its admitted rows and
application evidence. Rendering and settlement consume the delivered set,
rather than settling a whole container from only its first rendered item.

Evidence: `crates/lash-core/src/runtime/logical_turn.rs`, and
`crates/lash-core/src/runtime/durable/head_commit.rs`.

### 12. Commands are idempotent by compare-and-set

A config transaction (ADR 0126) is written against the config revision its
submitter read. Each session command is admitted alone. An applied
transaction advances `config_revision` by one; other commits preserve it. A
transaction whose expected revision the config has moved past publishes no
config and settles with the typed `Stale { expected, actual }` outcome for its
submitter. Its resolution is recorded before publication, so a resumed owner
reads it. The applying commit records each batch's outcome in
`RuntimeCommitReceipt::command_outcomes`. A terminal command retains its first
receipt identity, submission digest, terminal cause and time, with
`settled_operation_key` linking to that original commit receipt. Reattachment
and equal-key resubmission read its recorded outcome after later commits
advance the head. The head revision compare-and-set, terminal rows and receipt
commit atomically. Command tombstones follow ordinary vacuum; a resubmission after
vacuum meets the revision check and cannot reapply the transaction. There is no
separate session-command completion marker. `RefreshToolCatalog` recomputes
live sources.

Evidence: `crates/lash-core-store/src/config_transaction.rs`,
`crates/lash-core/src/runtime/config_transaction.rs` and
`crates/lash-core-store/src/store/queued_work.rs`.

### 13. Confirmations, stated as laws

Sequence order is per-session commit order across admission families.
Turn selection stops at the first ineligible unaddressed row; command priority
applies only at turn boundaries. Recorded run admission survives resume.
Scope-close work cannot retain the run's turn admission. A frame switch and
its follow-on's mail commit together. Host deliveries follow their source
commit and deduplicate by stable keys.

### 16. Conformance laws

Conformance covers shared sequence allocation, command-lane precedence,
contiguous turn-lane selection, run admission resume, stale-fence refusal,
exactly one follow-on per frame switch, cancellation of withheld inputs
and config compare-and-set.

Store tiers are SQLite file, SQLite memory, and PostgreSQL. Laws run the
production runtime over a fault-injecting store with labelled commits, a
virtual clock and `SimNodes` (ADR 0132 §14). Current store laws live in
`crates/lash-conformance/src/conformance/session_ingress.rs` and
`crates/lash-conformance/src/conformance/runtime_persistence/ingress_integrity.rs`.
`crates/lash-durable-test/tests/frame_switch_crash_proof.rs` covers the
switch/follow-on crash boundary. The former shift-admission and withheld-input
registrations are retired; this list does not claim their full runtime matrix
or a two-build upgrade proof.

### A1. The ingress is the only way a turn starts

The caller submits with `session.send(input)`. Lash's durable engine owns
execution and continuation over the lash store under ADR 0132. Process-owned child-session turns execute under their
own process run through the shared turn kernel. There is no caller-owned turn
fast path past queued work.

### A2. The caller observes through a handle

`send` returns a handle with `events()` and `outcome()`. Cancellation withdraws
queued input or requests durable cancellation of its running run. Dropping the
handle stops observation, not execution. Outcomes distinguish answers, failure,
cancellation, and a parked run.

Evidence: `crates/lash/src/send.rs:634`, `:644`, and
`crates/lash-core/src/runtime/session_manager/session_init.rs:1`.

### A3. Continuation belongs to the engine

Program failure commits a failed outcome. Infrastructure faults resume the same
recorded run from its committed phases on the actor's next activation. Lash does not synthesize
settlement because a caller or worker disappears.

### A4. Parked is a generic state of an engine-executed turn

A parked run is visible and blocks ordinary new work until resolution.
Explicit control supports redrive, cancel, and fork. A park is neither a
program failure nor a fabricated answer. Retry policy is recorded data under
[ADR 0110](0110-the-engine-owns-process-recovery.md) and ADR 0132 §7.

Implementation: `crates/lash-core/src/runtime/durable/session.rs`
(`SessionParkReason`) and `crates/lash/src/send.rs` (parked outcomes).

### A5. Session commands, and the session model as durable config

The session's provider/model route and generation settings are persisted config.
Creation records them; commands change them at boundaries. The run records its
resolved execution snapshot. Durable input may carry a `RunSpec` whose overrides
apply to that run without overwriting the sticky session config.

A run states its own tool grants the same way (FIG-5093): `RunOverrides::tool_access`
is the run's tool authority, whole, recorded with its shape, so a resume or a
cold reopen of the run sees the same grants. A toolbox switch is therefore an
accepted input, never a config command that waits for the running run; the
session's `SetToolAccess` stays the default for every run that states none.
The capability refs a `RunSpec` names are recorded with the shape too, and
reach the host's deferred tool resolver, with the session and the run, on
every resolution the run asks for.

Evidence: `crates/lash-core-store/src/session_policy.rs`,
`crates/lash-core-store/src/run_spec.rs`,
`crates/lash-conformance/src/conformance/runtime_persistence/run_specs.rs`.
This is store-level run-spec evidence; the retired runtime registrations are
not current proof of tool resolution across redrive.

### A6. Everything on a sent input is durable data

Prompt context, protocol turn options, and run-spec data cross ingress as
serializable values. A live callback or plugin object cannot supply durable
turn input.

### A7. No injected prompts

Worker loss or failed infrastructure does not create a model-visible abort
marker. Committed history and explicit host input supply model context.

## Alternatives considered

A single physical admission table is unnecessary for shared ordering and run
binding; the existing tables share one counter and admission protocol.
Timestamp arbitration cannot order concurrent producers reliably. Producer priority can starve
earlier work. A strict command FIFO barrier delays
config updates and complicates checkpoints; class-level command priority keeps
FIFO within its lane and preserves the running turn's snapshot.

A follow-on held as a fact on the session head needs its own admission
priority, recovery count and head-ownership rule, and every reader of the
head has to honour it. Mailed as input in the switch's own commit, it needs
none of them: the switch moved the head to the new frame, so the follow-on
renders there whenever it runs, and the session's ordinary mail recovers it.
Config compare-and-set
against `head_revision` rejects patches merely because a turn commits;
`config_revision` changes only with config. Caller-driven inline turns require
another continuation owner; ingress and the engine supply one durable path.

## Consequences

Order spans both admission tables. Commands precede fresh turns at boundaries.
Recorded admission cannot widen on retry. Hosts observe durable handles, and
keyed host deliveries retain their original identity. Follow-ons survive worker loss
as mail their switch committed. Durable shape evolution follows the pre-1.0 freeze and
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).
