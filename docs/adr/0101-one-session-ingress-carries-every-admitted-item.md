# 0101: One session ingress carries every admitted item

## Status

Accepted.

## Context

Host input, process wakes, and session commands require durable admission,
comparable ordering, replay-safe settlement, and explicit cancellation outcomes.
A frame handoff also needs recovery without becoming ordinary queued input.

## Decision

A session has one logical durable ingress composed from its admission tables.
The engine drives admitted turns. Callers submit durable data and observe the
result; they do not own a turn's continuation.

### 1. The model

`pending_turn_inputs` holds host input. Each `queued_work_batches` row holds
one process wake or session command in `payload_json`; its `work_kind` CHECK
agrees with the payload.
`session_ingress_sequence` allocates one per-session `enqueue_seq` across both
admission families. The counter is allocated inside the producer transaction.
The command lane is selected by kind; input and wakes form the turn lane.

A selected row records `admitted_root` and `admitted_by`. Root and checkpoint
admissions are fenced writes under the session's `DriveFence`. Admission
delivers the row's store-to-engine ingress obligation. `IngressSettlement`
names the root and its completed inputs, completed batches, released rows, and
dropped rows. There is no durable claim token or queued-run ledger.

Selection is a recorded `AdmitRoot` step. The store records the selected result
on `session_roots` in the same transaction as binding rows. Re-execution reads
that result instead of choosing again. A checkpoint names its own recorded
step. A root's fenced commit or terminal write settles or releases its rows.
The session has at most one admitted unfinished root. A terminal root's owed
scope close holds no admission and does not block the next root.

Evidence: `crates/lash-core-store/src/store/admission_plan.rs:1`, `:31`, `:67`,
`crates/lash-core-store/src/store/root.rs`,
`crates/lash-sqlite-store/src/persistence/admission.rs`,
`crates/lash-postgres-store/src/postgres/runtime_persistence/admission.rs`, and
`crates/lash-store-sql/src/session_ingress.rs`.

### 2. What stays separate, and why

Process wake delivery outboxes and allocation floors belong to the process
registry. Receiver wake-redelivery fences outlive queue rows. Turn cancellation
is arbitrated through the keyed-promise contract of
[ADR 0039](0039-turn-cancellation-is-a-first-party-work-driver-primitive.md).
Engine lane serialization controls driving, not ingress order. The host's
`QueuedDrainPolicy` bounds composition. None is another turn ingress.

Evidence: `crates/lash-core-execution/src/runtime/process/registry.rs:112`,
`crates/lash-core-store/src/store/admission_plan.rs:332`, and
`crates/lash-core-execution/src/runtime/turn_queue.rs`.

### 3. The pending follow-on lives on the session head

A frame switch records `PendingFollowOn` atomically with its frame pointer in
`pending_follow_on_json` on `session_head`. It contains the
follow-on turn id, frame id, task, options, resolved run, chain depth, recovery
count, and the recovery bound of its logical run. It is not a queue item.

The turn id derives from the logical root and next physical-turn ordinal.
Only that follow-on's terminal commit clears the fact or replaces it with the
next link. Every head write preserves its frame as current; another turn's
commit or frame open is refused while it is owed. Its own checkpoint admission
can proceed. Fork heads owe no source follow-on.

Drive admission prioritizes owed follow-on recovery before the unfinished root,
commands, or fresh turn-lane work. The original run continues its chain inline.
A recovery root records its decision, `RecoverFollowOn`, between its seal and
its turn. The step's body raises the recovery count once in a fenced write, and
records the raised fact, the exhaustion, or that the head does not owe the
follow-on. A raised or exhausted answer also records the head the follow-on's
turn runs on and its turn index, and the step retains that head as an
admission retains its base. Replay drives the recorded answer on the recorded
head and index, and cannot raise the count twice.
A root records the host's `max_follow_on_recoveries` (default 3) when it
resolves, as `follow_on_recoveries` on its `ResolvedRun`. Every fact of the
logical run carries that record, so the chain carries the bound: every
recovery decides on the recorded bound, never on the bound of the host driving
it or of the host that committed the switch, and the recorded decision carries
it. Exhaustion commits
`FollowOnRecoveryExhausted` as a failed follow-on with its task delivered and
clears the fact. Chain depth also survives crashes. Cancellation answers the
follow-on's own task, rather than deferring it as undelivered ingress.

Definition carry prepares successor-frame engine edges before the SQL head CAS
and retains the complete closure after a committed switch. Guarded cleanup
uses frame retention and journal end evidence under
[ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md).

Evidence: `crates/lash-core-store/src/store/pending_follow_on.rs:20`, `:32`,
`:76`, `crates/lash-core/src/runtime/drive/admission.rs:213`,
`crates/lash-core/src/runtime/drive/root.rs:677`, and
`crates/lash-sqlite-store/src/persistence/session_commit.rs:14`.

### 4. Session commands are a lane applied at turn boundaries

Commands apply at idle or after a logical root finishes, ahead of fresh
turn-lane roots. They do not apply mid-turn, at checkpoints, or between the
physical turns of one logical run. An owed follow-on and an unfinished root
retain precedence; a parked root whose redrive is unsettled holds the command
lane as it holds inputs, with the typed, retryable `SessionRedriveUnsettled`,
because the unfinished root owns the head. A checkpoint does not treat an open
command as a barrier.

Commands are not admitted turn rows. The drive selects the leading command run
and settles it in the applying commit. The drive's fenced read of the run is
the commands' admission: it delivers each row's obligation, and a withdrawal
reaches a command only before that read. Every command applies alone, with its
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

`CompactContext { instructions }` is a command. It applies under its command
root's sealed fence, journals its summary, opens the frame and records usage
with settlement in one commit. Its typed outcome is `Opened`,
`NothingToCompact`, or `Failed`. A settled replay adopts the published head and
cannot open another frame. A storeless runtime serializes direct compaction
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
runtime events, plugin state, and queued turns ride the command's commit, and a
task journals its effects under the command's own session-operation scope. A frame
open opens its frame and restarts the live interpreter from the seed. A command
that cannot apply, including one whose commit exceeds the commit budget,
settles with its typed refusal, so the lane never waits on it. The one command
that cannot settle is one whose bare settlement exceeds the budget, on a head a
host lowered the budget below (ADR 0058): its drive stops at the typed
refusal, and the command settles once the host raises the budget.

Head-writing host commands return their typed `SessionCommandOutcome` through
`SessionCommandSettlement::Applied { receipt, outcome }`. Config transactions
carry their typed applied, stale or refused result as described in §12;
a catalog refresh answers `Durable`. A host submits with a stable idempotency
key and gets a durable receipt (`SessionCommandAdmin::submit`). A resubmission
with equal key and content returns the first receipt while the command is open
or retained as a tombstone. `settle` answers the recorded result, `Cancelled`,
or `Pending` with the receipt when the drive has not applied the command by
the deadline, and a host reattaches by the receipt. The convenience calls
(`append_messages`, `append_session_nodes`, `open_agent_frame`, the plugin
operations, `compact_context`) submit and await. Dropping an await does not
withdraw the command; `withdraw` does, transactionally, and answers
`AlreadyAdmitted` once a drive read it. A host's cancel of a plugin task a
drive admitted resolves the task's cancel signal, a keyed promise
(`SessionCommandCancelSignal`) under the command's session-operation scope that
only a host's cancel writes; a cancel of a command that already settled
finds its settlement and writes nothing. The signal is a durable request,
never a decision: the drive peeks it before the task runs, fires the task's
cancellation token when the cancel lands, and peeks it again the moment the
task's code returns. A cancel requested by then settles the command
`PluginOperationCommandOutcome::Cancelled` with nothing of the task
committed; otherwise it settles with the task's own outcome. The settling
commit is the one record of that decision (FIG-4453): a drive that dies
before it leaves nothing decided, and its redrive runs the task's code again
under the same live signal, or none of it once the cancel was requested. A
cancel landing after the drive's last peek reaches nothing, and the
settlement says so. Neither the withdrawal nor the cancel takes the runtime
writer, which the drive applying the commands holds. The runtime
writer is never held while a settlement is awaited. A command root, once it drained the lane, writes its
`RootTerminalCause::CommandsApplied` terminal and arms its scope close, so its
journal is retired like a turn root's.

Evidence: `crates/lash-core/src/runtime/drive/admission.rs:213`,
`crates/lash-core/src/runtime/session_api.rs:1375`,
`crates/lash-core/src/runtime/compact_context.rs:1`,
`crates/lash-core/src/runtime/host_commands.rs:1`,
`crates/lash-core/src/runtime/host_commands/task_cancel.rs:1`,
`crates/lash-core/src/runtime/drive/root.rs` (`run_commands_root`),
`crates/lash/src/admin/host_commands.rs:1`, and
`crates/lash-core-store/src/store/mod.rs:1591`.

### 5. Ordering and composition

Within the turn lane, the shared `enqueue_seq` is the order; there is no input
or wake priority. Commands are a separate lane and do not establish a turn-lane
stop. Clock values can bound age; they cannot decide order. Lash implements no
authentication or security policy.

Evidence: `crates/lash-core-store/src/store/admission_plan.rs:285`,
`crates/lash-core/src/runtime/drive/admission.rs:289`,
`crates/lash-core-execution/src/runtime/park.rs::turn_lane_head`, and
`crates/lash-core-store/src/store/queued_work.rs:244`.

#### 5.1 Turn addressing is immutable intent

An input's submitted delivery is written once and never rewritten.
`Turn { turn_id: T, min_boundary }` is eligible only at T's admitted checkpoints
while T runs. Once T is not running it is eligible as next-turn input at its
existing sequence position, by rule rather than stored delivery mutation: an
open row is next-turn input when its delivery is next-turn, or when it
addresses a turn other than the running one, and with no turn running every
open row is. A root's release hands an accepted row back open in the state its
delivery names, and deferral at a cancel writes nothing.

Admission accepts the address only if T is this session's running turn or has
ended. T is running when it is a physical turn of the unfinished root, of a
member that root's admission composed, or of the follow-on the head owes. T
has ended when its final commit or its root's terminal is recorded. An unknown
turn, another session's running turn included, is refused with
`StoreError::IngressTurnAddressUnknown` (`TurnAddressUnknown` to the runtime)
before any row or sequence number is allocated. A resubmission of an admitted
row is answered by its digest before the address is consulted.

Evidence: `crates/lash-core-store/src/turn_input_vocabulary.rs:292`,
`crates/lash-core-store/src/store_backend_support/queued_work_admission.rs:77`,
`crates/lash-store-sql/src/turn_ingress/pending_inputs.rs`
(`earliest_next_turn_candidate_seq`, `release_root`), and
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
next-turn input is its own root and a cancel of one never reaches another.

Evidence: `crates/lash-core-store/src/store/queued_work.rs`
(`select_turn_work_indices`),
`crates/lash-core-store/src/store/admission_plan.rs`
(`plan_next_turn_input_admission`),
`crates/lash-core-store/src/queued_drain_policy.rs`, and
`crates/lash-conformance/src/conformance/queued_input_roots.rs`.

One root answers every input it admits, at idle or at its checkpoints. Each
input retains its own application evidence even when inputs share a root's
answer.

### 6. Render order

Within one turn's delivered set, host messages precede wake causes, and each
kind preserves its sequence order. Selection order and presentation order are
separate. A wake is not host-authored input and carries no host turn options.

Evidence: `crates/lash-core/src/runtime/logical_turn.rs`,
`crates/lash-core/src/runtime/turn_loop/commit.rs`, and
`crates/lash-conformance/src/conformance/cancelled_turn_withheld_input.rs`.

### 7. Admission, deferral and redrive

An interrupted selection replays its root's recorded admission. `Defer`
releases the row binding at its own sequence position; subsequent admission
recomposes open rows. A stale drive fence refuses admission and settlement
without writing. Settlement checks that every named row belongs to the root.
Completed input carries application evidence. Root terminal writes release
remaining bindings, so terminal roots cannot retain admitted rows.

Evidence: `crates/lash-core-store/src/store/admission_plan.rs:67`, `:197`,
`crates/lash-core-store/src/store/root.rs`, and
`crates/lash-sqlite-store/src/persistence/ingress_settlement.rs`.

### 8. Dedup, digest and tombstones

Admission records an immutable submission digest. Equal source key and digest
returns the existing item, open or terminal. A changed digest returns a typed
content conflict for every kind, without silently adopting different content:
`PendingTurnInputSourceKeyConflict` for input and
`QueuedWorkSourceKeyConflict` for queued work. Wake identity covers its process
fact (target session, process, sequence, event type, input, authority and
cause) rather than host-configured delivery policy, merge key or delivery
metadata. A command's digest covers the command, its delivery policy, its
authority and its merge key. A config transaction command's digest covers
its id, the revision it was written against and its ordered commands, never
the reducer identities ingress stamped on it, so a resubmission from another
build is the same request; the lane refuses a changed one as
`ConfigSubmitError::ChangedContent`. System source-key namespaces belong to
their item kinds.
Admission reserves `command:` for session commands and `process:` for process
wakes. Input and queued-work producers enforce this before deduplication or
allocation, and a mixed input request is refused atomically. Plugin commands
queue their inputs as one request, with generated keys under `input:command:`;
an admission refusal retains its typed cause through command settlement to the host.

Terminal items stay in place as tombstones. Tombstones preserve kind, source
key, sequence, submitted delivery, digest, terminal cause, and terminal time,
with no admission binding. A queued-work tombstone records `terminal_cause`
and `terminal_at_ms`; an input's terminal state owns its cause and terminal time, decoded together
from `state`, `ingress_json` and `terminal_at_ms`. Cancelled items cannot reopen on retry. Terminal causes
distinguish delivered input or wake, applied command, stale config revision,
and cancellation.
Open-row selection excludes tombstones. Host vacuum removes queued-work
tombstones and withdrawn input; an input a root took keeps its tombstone
beside that root's terminal evidence until session deletion.

Evidence: `crates/lash-core-store/src/store/ingress_terminal.rs`,
`crates/lash-core-store/src/store_backend_support/queued_work_admission.rs:29`,
`crates/lash-store-sql/src/turn_ingress/queued_batches.rs`
(`settle_admitted`, `settle_command`, `withdraw_open`, `delete_tombstones`), and
`crates/lash-conformance/src/conformance/runtime_persistence/ingress_integrity.rs`.

### 9. The floor invariant

Every terminal wake transition raises the receiver redelivery floor to at least
its sequence in the same transaction: delivery, drop, host withdrawal, or the
refusal of a changed process fact under the wake's process and sequence. That
refusal stores no row and leaves the receiver's own wake untouched; its
transaction commits the floor before `QueuedWorkSourceKeyConflict` is returned,
so the sender can only acknowledge a conflict whose floor is durable. The two
stores share no transaction: a sender that loses its acknowledgement retries
and reaches the same refusal.
`Defer` retains position and does not advance the floor. Until host vacuum a
redelivery answers the wake's tombstone; after it, redelivery at or below the
floor is refused `ProcessWakeSequenceRewound`. Neither recreates work. Process-owned allocation floors
and receiver floors have distinct responsibilities.

Evidence: `crates/lash-core-store/src/store/admission_plan.rs:332`,
`crates/lash-sqlite-store/src/persistence/queued_work.rs:110`, and
`crates/lash-postgres-store/src/postgres/runtime_persistence/queued_work.rs`.

### 10. Cancel by author

A turn cancel applies its accepted undelivered-input policy to host input
addressed to that turn. `Defer` is the default and writes nothing; `Drop`
records cancellation. Once the turn's root has terminal evidence, the root's
terminal write has already applied the disposition, and a later teardown of
that turn reaches no input. Other held input is released. Every held wake is deferred at its existing
position with its floor unchanged. `TurnCancelInputOutcome` records affected
inputs and affected wakes with their disposition. Host withdrawal may remove
undelivered queued work, including wakes; wake withdrawal raises its floor.
Dropping an observation handle cancels nothing.

Evidence: `crates/lash-core-store/src/turn_control_vocabulary.rs:54`, `:100`,
`crates/lash-core-store/src/store/admission_plan.rs:376`, and
`crates/lash-sqlite-store/src/persistence/ingress_settlement.rs:153`.

### 11. Recompose and render admitted work

Work retained across a physical-turn follow-on carries its admitted rows and
application evidence. Rendering and settlement consume the delivered set,
rather than settling a whole container from only its first rendered item.

Evidence: `crates/lash-core/src/runtime/logical_turn.rs:66`, and
`crates/lash-core/src/runtime/turn_loop/commit.rs`.

### 12. Commands are replay-safe by compare-and-set

A config transaction (ADR 0126) is written against the config revision its
submitter read. Each session command is admitted alone. An applied
transaction advances `config_revision` by one; other commits preserve it. A
transaction whose expected revision the config has moved past publishes no
config and settles with the typed `Stale { expected, actual }` outcome for its
submitter. Its resolution is recorded before publication, so a redrive
replays it. The applying commit records each batch's outcome in
`RuntimeCommitReceipt::command_outcomes`. A terminal command retains its first
receipt identity, submission digest, terminal cause and time, with
`settled_operation_key` linking to that original commit receipt. Reattachment
and equal-key resubmission read its recorded outcome after later commits
advance the head. The head revision compare-and-set, terminal rows and receipt
commit atomically. Command tombstones follow ordinary vacuum; replay after
vacuum meets the revision check and cannot reapply the transaction. There is no
separate session-command completion marker. `RefreshToolCatalog` recomputes
live sources.

Evidence: `crates/lash-core-store/src/config_transaction.rs`,
`crates/lash-core/src/runtime/config_transaction.rs` and
`crates/lash-core-store/src/store/queued_work.rs`.

### 13. Confirmations, stated as laws

Sequence order is per-session commit order across admission families.
Turn selection stops at the first ineligible unaddressed row; command priority
applies only at turn boundaries. Recorded root admission survives redrive.
Scope-close work cannot retain the root's turn admission. Follow-on frame and
recovery bounds survive reopen. Wake terminal writes and floor writes are atomic.

### 16. Conformance laws

Conformance covers shared sequence allocation, command-lane precedence,
contiguous turn-lane selection, root admission replay, stale-fence refusal,
follow-on head invariants and bounded recovery, cancellation of withheld inputs
and wakes, wake floors, and config compare-and-set.

Store tiers are SQLite file, SQLite memory, and PostgreSQL. Host tiers are the
in-process Restate server double, live Restate, and lash-sim's in-process effect
host. Upgrade proofs use synthetic-next. Evidence lives in
`crates/lash-conformance/src/conformance/session_ingress.rs`,
`crates/lash-conformance/src/conformance/drive_admission.rs`,
`crates/lash-conformance/src/conformance/runtime_persistence/pending_follow_on.rs`,
`crates/lash-conformance/src/conformance/cancelled_turn_withheld_input.rs`,
`crates/lash-conformance/src/conformance/runtime_persistence/ingress_integrity.rs`,
and `crates/lash-restate-test/tests/follow_on_crash_replay.rs`.

### A1. The ingress is the only way a turn starts

The caller submits with `session.send(input)`. Restate owns production driving
and continuation under
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md).
SQL stores persist state. Process-owned child-session turns execute under their
own process run through the shared turn kernel. There is no caller-owned turn
fast path past queued work.

### A2. The caller observes through a handle

`send` returns a handle with `events()` and `outcome()`. Cancellation withdraws
queued input or requests durable cancellation of its running root. Dropping the
handle stops observation, not execution. Outcomes distinguish answers, failure,
cancellation, and a parked root.

Evidence: `crates/lash/src/send.rs:634`, `:644`, and
`crates/lash-core/src/runtime/session_manager/session_init.rs:1`.

### A3. Continuation belongs to the engine

Program failure commits a failed outcome. Infrastructure faults redrive through
the engine under the same recorded root and journal. Lash does not synthesize
settlement because a caller or worker disappears.

### A4. Parked is a generic state of a driver-run turn

A parked root is visible and blocks ordinary new work until resolution.
Explicit control supports redrive, cancel, and fork. A park is neither a
program failure nor a fabricated answer. Restate owns retry policy under
[ADR 0110](0110-the-engine-owns-process-recovery.md).

Evidence: `crates/lash-core/src/runtime/drive/admission.rs:112`, and
`crates/lash-core-execution/src/runtime/park.rs`.

### A5. Session commands, and the session model as durable config

The session's provider/model route and generation settings are persisted config.
Creation records them; commands change them at boundaries. The root records its
resolved execution snapshot. Durable input may carry a `RunSpec` whose overrides
apply to that run without overwriting the sticky session config.

Evidence: `crates/lash-core-store/src/session_policy.rs:125`,
`crates/lash-core-store/src/run_spec.rs`, and
`crates/lash-conformance/src/conformance/run_spec_drive.rs`.

### A6. Everything on a sent input is durable data

Prompt context, protocol turn options, and run-spec data cross ingress as
serializable values. A live callback or plugin object cannot supply durable
turn input.

### A7. No injected prompts

Worker loss or failed infrastructure does not create a model-visible abort
marker. Committed history and explicit host input supply model context.

## Alternatives considered

A single physical admission table is unnecessary for shared ordering and root
binding; the existing tables share one counter and admission protocol.
Timestamp arbitration cannot order concurrent producers reliably. Input-before-
wake priority can starve earlier wakes. A strict command FIFO barrier delays
config updates and complicates checkpoints; class-level command priority keeps
FIFO within its lane and preserves the running turn's snapshot.

A queued frame-handoff item can be withdrawn, overtaken, or rendered in another
frame. The head fact carries its frame and continuation together. An unbounded
follow-on retry can block the whole session indefinitely. Config compare-and-set
against `head_revision` rejects patches merely because a turn commits;
`config_revision` changes only with config. Caller-driven inline turns require
another continuation owner; ingress and the engine supply one durable path.

## Consequences

Order spans both admission tables. Commands precede fresh turns at boundaries.
Recorded admission cannot widen on retry. Hosts observe durable handles, and
wake cancellation preserves the receiver floor. Follow-ons survive worker loss
as head obligations. Durable shape evolution follows the pre-1.0 freeze and
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).
