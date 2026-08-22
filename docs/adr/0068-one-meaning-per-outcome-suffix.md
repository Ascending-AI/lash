# One meaning per outcome-type suffix

## Status

Accepted.

## Context

The facade exports eighty-nine types whose names end in a "what happened"
suffix: `Result`, `Outcome`, `Report`, `Summary`, `Disposition`, `Receipt`,
`Status`. Seven suffixes, no rule. A host reading two exports side by side could
not tell from the names what the difference between them was, because there
wasn't one — the suffix recorded whichever word the author reached for that day.

The incoherence was not cosmetic. Four shapes of it were load-bearing:

* **`Result` collided with the language.** `TurnResult`, `ToolResult`,
  `RuntimeCommitResult`, `TriggerIngressResult` and fifteen more sat next to
  `crate::Result`, `TriggerEffectResult` (a genuine `std::Result` alias), and
  every `-> Result<T, E>` in the crate. In a codebase where `Result` means "the
  fallible-return type", spending the word on domain nouns costs a reader a
  disambiguation on every line.

* **`Disposition` meant three unrelated things.** `RecoveryDisposition` is a
  producer-declared *contract* stated at registration. `LoserDisposition` is a
  *policy* the caller chooses before a group runs. `TriggerMutationDisposition`,
  `QueuedWorkWakeDisposition` and `ProcessRecoveryAttemptDisposition` are
  *outcomes* observed after the fact. Three tenses under one suffix: an input, a
  policy, and a result.

* **`Report` was applied to single items.** `TriggerDeliveryEmitReport` and
  `RemoteTriggerDeliveryEmitReport` each describe exactly one delivery —
  occurrence id, subscription id, process id, outcome. The aggregate over many
  deliveries is `TriggerEmitReport`, which contains a `Vec` of them. One suffix,
  both the element and the collection.

* **Adjacent pairs read as duplicates.** `PendingTurnInputCancelOutcome` (the
  closed enum) and `PendingTurnInputCancelResult` (the struct pairing a target
  with that enum) differ in the suffix alone, and the suffix does not say which
  is which. `TurnCancelOutcome`/`TurnCancelReceipt` is the same relationship,
  spelled a third way.

`Summary` carried its own failure: it was the suffix reached for whenever a type
was a *read projection* — `ProcessHandleSummary` is a handle rendering,
`ExecutionSummary` is four numbers about one turn, `RemoteToolCallSummary` is a
call record. None of them summarize anything; the word only signalled "this is
some data about that thing", which is what a type is.

## Decision

**A type name's final noun states what kind of answer the type is, and each of
the four permitted nouns has exactly one meaning.**

| Suffix | Meaning |
| --- | --- |
| **Receipt** | Durable acknowledgement that a request was accepted or recorded. Says nothing about the final state. |
| **Outcome** | The terminal state of one operation. Canonically a closed enum; a struct where the operation has exactly one terminal shape. |
| **Report** | An aggregate over many items — sweeps, maintenance passes, batch emits: counts plus per-item detail. |
| **Status** | A point-in-time observation of something still in flight. |

Three suffixes are retired:

* **`Result`** is reserved for `std::Result`, including idiomatic aliases such
  as `TriggerEffectResult = Result<TriggerCommandOutcome, TriggerOperationError>`
  and the crate's own `Result`. No domain type ends in it.
* **`Disposition`** is retired outright. In practice every use was an Outcome, a
  Receipt, a declared contract, or a policy, and the suffix hid which.
* **`Summary`** is retired. A genuine aggregate is a Report. Everything else was
  a read projection, and a projection is named for *what it projects* — a domain
  noun, or `...View` — never for the fact that it is data.

A type that is not a "what happened" answer at all does not take one of the four
nouns. It is named for what it is: a caller-chosen policy is a `...Policy`, a
producer-declared contract is a `...Contract`, a rendering of a durable row is a
`...View` or the domain noun itself. The four nouns are exhaustive *for outcome
types*, not for the crate.

### What this decision does not touch

Type names only. Three neighbouring surfaces stay exactly as they are, and the
distinction is deliberate:

* **Serde field and variant names are wire.** `TriggerMutationReceipt` keeps its
  `disposition` field, and every `#[serde(rename_all)]` variant keeps its
  spelling, because renaming them would be a payload change wearing a
  vocabulary change's clothes. The clearest case is the content-block variant
  `LlmContentBlock::ToolResult` — and its `PartKind`, `TraceContentBlock` and
  `RemoteLlmContentBlock` siblings — which survives the retirement of the
  `ToolResult` *type* it never had anything to do with: it is the message block
  carrying a tool's output back to the model, it serializes as `tool_result`,
  and every provider on the wire spells it that way.
* **Module paths are paths.** `lash::remote::turn_result` and
  `lash_core::tool_result` keep their names; they are import paths a host has
  written down, and the suffix rule is about answers, not modules.
* **Local bindings and test-function names are neither.** They are not API.

The wave itself moves the exported surface — every type a host can name,
whether it is re-exported from the `lash` root or reached as a `lash_core`
contract symbol. The line is *nameability by a host*, not the module a type
happens to live in: `TurnResultSummary` is handed to plugin hooks,
`LiveReplayResult` is returned by a store trait a host implements, and
`SessionObservedProcessResult` rides in a session snapshot, so all three are
renamed here even though none is a `lash::` root export.

What is carved out is what a host cannot write. Crate-internal types still
carrying a retired suffix — `RecoveryCompletionDisposition`, the private tuple
alias `LiveReplayRecvResult`, and their kin — are governed by the same rule and
are renamed as their code is touched; paying for a second thousand-line
mechanical diff to reach names no host can write is not worth a reviewer's
afternoon.

### Two near-neighbours, kept apart on purpose

`ToolOutcome` (formerly `ToolResult`) is what a tool body returns: `Done` or
`Pending`. `ToolCallOutcome` is how the call settled: `Success`, `Failure`,
`Cancelled`. They are terminal states of two different operations — the body's
return, and the call's disposition — so both keep `Outcome`, and neither is
renamed into the other. `ToolRetryStatus` (formerly `ToolRetryDisposition`) is
likewise distinct from the pre-existing `ToolRetryPolicy`: the policy is what the
host configured, the status is where one failure's retries actually stand.

## Alternatives considered

* **Keep the suffixes, document what each already means.** Rejected: there was
  no consistent existing meaning to document. Writing one down without moving
  the types would have produced a glossary that the majority of the surface
  contradicts, which is worse than no glossary.

* **Rename incrementally, one domain per release.** Rejected: the value is
  entirely in the invariant "the suffix tells you the kind", and an invariant
  that holds for the trigger types but not the process types is not an
  invariant. A partial wave also charges hosts two breaking migrations for one
  change.

* **Allow `Summary` for read projections.** Rejected: it is exactly the usage
  that produced `ProcessHandleSummary` and `ExecutionSummary`. The word says
  "less than the whole" when what is meant is "a specific projection", and the
  projection's own name carries far more information.

* **Merge the duplicate-looking pairs instead of renaming them.** Rejected here
  and deferred: `PendingTurnInputCancelOutcome`/`...Receipt` and the trigger
  emit pair are separate types for real reasons (one is a closed enum, the other
  binds it to an addressed target). Whether any pair should collapse is a
  semantic question, and this wave is mechanical by construction — no type is
  merged, no field changes, no behaviour moves.

## Consequences

* Forty-six exported types are renamed and one satellite (`ToolResultDone` →
  `ToolOutcomeDone`) follows its parent. Forty-six were already canonical and
  keep their names. The appendix is the whole table.
* This is a breaking release for every host that names one of the renamed types.
  Nothing is deprecated and re-exported under the old name: a shim would keep
  both vocabularies alive, which is the state this ADR exists to end.
* No serialized payload changes. The wave is type identifiers only, so a store
  written before it reads identically after it.
* No versioned surface is bumped, and the version-bump gate says so by name.
  `REMOTE_PROTOCOL_VERSION` stays 41, `SESSION_NODE_BODY_SCHEMA_VERSION` stays
  1, and `PROCESS_REGISTRATION_FAMILY_VERSION` stays 4: the renames leave every
  serde field name, variant name, and emitted fingerprint tag byte-identical, so
  a bump on any of the three would publish an incompatibility that does not
  exist — on the remote protocol it would tell peers to refuse each other over a
  wire neither of them changed. The gate still sees a change, because it
  projects the text of the guarded items and identifiers are part of that text,
  so this wave is excused by three burned entries in
  `IDENTIFIER_RENAME_BASELINES` in `scripts/check_version_bumps.py`. Each entry
  pins a surface key to the sha256 of that surface's guard signature at this
  diff's head, in the same idiom as the registration baselines beside it: the
  exemption is reachable only by these exact guarded bytes, so no later refactor
  — not even another identifier-only one — inherits it, and the next real shape
  change on any of the three owes its bump as before.
* New exports are held to the rule at review. A type ending in `Result`,
  `Summary`, or `Disposition` is a review finding; a type ending in `Report`
  that describes one item is the same finding.
* The store-maintenance types are classified here and redesigned elsewhere.
  `GcReport`, `VacuumReport`, `AttachmentReclamationReport` and
  `ProcessPruneReport` are all genuine aggregates and keep their names; FIG-1494
  and FIG-1505 rework what they contain, on top of this vocabulary rather than
  around it.

## Appendix: reclassification table

Every host-nameable exported type whose name ended in `Result`, `Outcome`,
`Report`, `Summary`, `Disposition`, `Receipt`, or `Status` — the `lash` facade's
re-exports and the `lash_core` contract symbols a host reaches through them — in
alphabetical order of the old name.

| Old name | New name | Why |
| --- | --- | --- |
| `AppendSessionNodesResult` | `AppendSessionNodesOutcome` | Closed enum: `Appended` or `StaleBranch` — the terminal states of one append. |
| `AttachmentReclamationReport` | *unchanged* | Sweep aggregate: scanned, reclaimed, failed ids, detector list. |
| `DeploymentDrainStatus` | *unchanged* | Point-in-time read of a deployment that is still draining. |
| `DirectLlmResult` | `DirectLlmOutcome` | Terminal value of one direct call; the operation has a single terminal shape. |
| `ExecutionSummary` | `TurnExecutionMetrics` | Not a summary: four measured facts about one turn's execution. Named for what it projects. |
| `ForkSessionResult` | `ForkSessionReceipt` | Durable identity acknowledging a recorded fork. |
| `GcReport` | *unchanged* | Sweep aggregate: root, retained, and deleted blob counts. |
| `GenerationDisposition` | `GenerationReceipt` | The adapter's acknowledgement of what it accepted from one request's generation intent. |
| `GenerationOptionDisposition` | `GenerationOptionOutcome` | Closed enum of the terminal fates of one requested option. |
| `LiveReplayResult` | `LiveReplayOutcome` | Closed enum: the replayed events, or a gap. |
| `LiveReplaySubscribeResult` | `LiveReplaySubscribeOutcome` | Closed enum: subscribed, or a gap. |
| `LoserDisposition` | `LoserPolicy` | Not an answer: a caller-chosen policy stated before the group runs. |
| `PendingTurnInputCancelOutcome` | *unchanged* | Closed enum of terminal cancel states. |
| `PendingTurnInputCancelResult` | `PendingTurnInputCancelReceipt` | **Collision resolved.** Binds the addressed target to its outcome; the enum keeps `Outcome`. |
| `PendingTurnInputSuffixCancelOutcome` | *unchanged* | Closed enum of terminal states for a suffix cancel. |
| `ProcessAdmissionReport` | *unchanged* | Aggregate: admitted ids plus per-row deferral detail. |
| `ProcessCancelSummary` | `ProcessCancelReceipt` | Acknowledges a cancel request and names the state the process was left in. |
| `ProcessDrainReport` | *unchanged* | Aggregate: abandoned ids plus per-row deferral detail. |
| `ProcessEffectOutcome` | *unchanged* | Closed enum of terminal states per process operation. |
| `ProcessEventAppendResult` | `ProcessEventAppendReceipt` | Acknowledges a recorded event and any wake it armed. |
| `ProcessHandleSummary` | `ProcessHandleView` | Read projection: the handle rendering of a process row. |
| `ProcessLeaseClaimOutcome` | *unchanged* | Closed enum: acquired, or busy with the observed holder. |
| `ProcessLiveReferenceSummary` | `ProcessLiveReferenceView` | Read projection of a definition's live references. |
| `ProcessPruneReport` | *unchanged* | Retention aggregate: rows, events, and deliveries deleted. FIG-1505 builds on this vocabulary. |
| `ProcessRecoveryAttemptDisposition` | `ProcessRecoveryAttemptOutcome` | Closed enum of how one recovery attempt ended. |
| `ProcessSessionDeleteReport` | *unchanged* | Aggregate of per-category deletion counts. |
| `ProcessStatus` | *unchanged* | Point-in-time lifecycle state of a process row. |
| `QueuedWorkWakeDisposition` | `QueuedWorkWakeOutcome` | Closed enum of how one wake ended. |
| `RecoveryDisposition` | `RecoveryContract` | Not an answer: a producer-declared contract stated at registration (ADR 0019). |
| `RemoteAttemptOutcome` | *unchanged* | Closed enum of terminal attempt states. |
| `RemoteExecutionSummary` | `RemoteTurnExecutionMetrics` | Wire mirror of `TurnExecutionMetrics`. |
| `RemoteGenerationDisposition` | `RemoteGenerationReceipt` | Wire mirror of `GenerationReceipt`. |
| `RemoteGenerationOptionDisposition` | `RemoteGenerationOptionOutcome` | Wire mirror of `GenerationOptionOutcome`. |
| `RemotePersistProcessEnvResult` | `RemotePersistProcessEnvReceipt` | Acknowledges a persisted environment and names its ref. |
| `RemoteProcessAwaitResult` | `RemoteProcessAwaitOutcome` | The await's terminal output. |
| `RemoteProcessCancelResult` | `RemoteProcessCancelReceipt` | Wire mirror of `ProcessCancelReceipt`. |
| `RemoteProcessSignalResult` | `RemoteProcessSignalReceipt` | Acknowledges a recorded signal event. |
| `RemoteProcessStartResult` | `RemoteProcessStartReceipt` | Acknowledges a started process; start is not completion. |
| `RemoteProcessStatus` | *unchanged* | Wire mirror of `ProcessStatus`. |
| `RemoteProcessSummary` | `RemoteProcessHandleView` | Wire mirror of `ProcessHandleView`; field-for-field the same projection. |
| `RemoteRecoveryDisposition` | `RemoteRecoveryContract` | Wire mirror of `RecoveryContract`. |
| `RemoteToolCallOutcome` | *unchanged* | Closed enum: success, failure, cancelled. |
| `RemoteToolCallSummary` | `RemoteToolCallRecord` | Read projection of one call, matching core's `ToolCallRecord`. |
| `RemoteTriggerDeliveryEmitOutcome` | *unchanged* | Closed enum of one delivery's terminal states. |
| `RemoteTriggerDeliveryEmitReport` | `RemoteTriggerDeliveryEmitReceipt` | **Collision resolved.** One delivery, not an aggregate; the aggregate is `RemoteTriggerEmitReport`. |
| `RemoteTriggerEmitReport` | *unchanged* | Aggregate over the deliveries one occurrence produced. |
| `RemoteTriggerRegisterSubscriptionResult` | `RemoteTriggerRegisterSubscriptionReceipt` | Acknowledges a recorded subscription. |
| `RemoteTriggerTargetSummary` | `RemoteTriggerTarget` | Read projection: the target's own definition. Domain noun. |
| `RemoteTurnCancelOutcome` | *unchanged* | Closed enum of terminal cancel states. |
| `RemoteTurnCancelReceipt` | *unchanged* | Acknowledges an addressed cancellation gate. |
| `RemoteTurnOutcome` | *unchanged* | Closed enum: finished, frame switch, stopped. |
| `RemoteTurnResult` | `RemoteTurnReport` | Wire mirror of `TurnReport`. **CONTESTED** — see `TurnResult`. |
| `RemoteTurnStatus` | *unchanged* | Derived projection of `RemoteTurnOutcome`: computed on encode, checked against the outcome on decode. It states no fact the outcome does not already carry, so it has no `InProgress` — a turn report exists only once the turn has a terminal outcome (FIG-1757). |
| `RemoteTurnUsageSummary` | `RemoteTurnUsageReport` | Aggregate: parent usage, per-child ledger entries, total. |
| `ResolveOutcome` | *unchanged* | Closed enum of terminal resolve states. |
| `RuntimeCommitResult` | `RuntimeCommitReceipt` | Durable acknowledgement of one commit, replayed verbatim on receipt replay. |
| `RuntimeEffectOutcome` | *unchanged* | Closed enum of terminal states per runtime effect. |
| `RuntimeEffectReplayMismatchSummary` | `RuntimeEffectReplayMismatchReport` | Aggregate: divergent-path count plus the first paths. |
| `SelectedQueuedWorkClaimOutcome` | *unchanged* | Terminal state of one selection resolve; single terminal shape. |
| `SelectedQueuedWorkDrainOutcome` | *unchanged* | Terminal state of one exact drain. |
| `SessionCommandReceipt` | *unchanged* | Acknowledges an enqueued session command. |
| `SessionDeleteReport` | *unchanged* | Aggregate of what a session deletion removed. |
| `SessionExecutionLeaseClaimOutcome` | *unchanged* | Closed enum: acquired, or busy with the holder. |
| `SessionObservedProcessOutcome` | *unchanged* | Closed enum of terminal states for one observed process. |
| `SessionObservedProcessResult` | `SessionObservedProcessReceipt` | Binds one addressed process to its `SessionObservedProcessOutcome`; same shape as `PendingTurnInputCancelReceipt`. |
| `StoreSchemaOutcome` | *unchanged* | Closed enum: ready, refused, undecided. |
| `StoreSchemaStatus` | *unchanged* | Point-in-time read of every schema-carrying database. |
| `ToolAttemptResult` | `ToolAttemptOutcome` | Closed enum of how one provider attempt ended: done, or parked. |
| `ToolIntentExecutionOutcome` | *unchanged* | Closed enum: executed, refused, protocol-refused. |
| `ToolIntentIngressOutcome` | *unchanged* | Closed enum: admitted, or refused. |
| `ToolRestoreReport` | *unchanged* | Aggregate: adopted generation plus the orphaned tool ids. |
| `ToolResult` | `ToolOutcome` | Closed enum of how a tool body returned. **CONTESTED** — `ToolBodyOutcome` would separate it further from `ToolCallOutcome`, at the cost of the ergonomics of the most-written name in tool authoring. |
| `ToolResultDone` | `ToolOutcomeDone` | Satellite: the completed variant's payload, following its parent. |
| `ToolRetryDisposition` | `ToolRetryStatus` | Where one failure's retries stand right now. Distinct from the pre-existing `ToolRetryPolicy`. |
| `TraceLanguageExecutionStatus` | *unchanged* | Point-in-time execution state, including `Running`. |
| `TraceLashlangNodeStatus` | *unchanged* | Point-in-time node state, including `Unobserved` and `Running`. |
| `TriggerCommandOutcome` | *unchanged* | Closed enum of terminal states per trigger command. |
| `TriggerDeliveryEmitOutcome` | *unchanged* | Closed enum of one delivery's terminal states. |
| `TriggerDeliveryEmitReport` | `TriggerDeliveryEmitReceipt` | **Collision resolved.** One delivery; the aggregate is `TriggerEmitReport`. |
| `TriggerDeliveryReservationStatus` | `TriggerDeliveryReservationOutcome` | Not in flight: reserved or already-reserved is how the reservation attempt ended. |
| `TriggerEffectResult` | *unchanged* | A genuine `std::Result` alias — the reserved use of the word. |
| `TriggerEmitReport` | *unchanged* | Aggregate over the deliveries one occurrence produced. |
| `TriggerIngressResult` | `TriggerIngressReceipt` | Acknowledges a recorded occurrence and the reservations it armed. |
| `TriggerMutationDisposition` | `TriggerMutationOutcome` | **Collision resolved.** Closed enum of how one mutation landed; the receipt keeps `Receipt`. |
| `TriggerMutationReceipt` | *unchanged* | Durable acknowledgement of one applied mutation. |
| `TriggerTargetSummary` | `TriggerTarget` | Read projection: the target's own definition. Domain noun. |
| `TurnCancelOutcome` | *unchanged* | **Collision resolved, no rename.** Closed enum of terminal cancel states; the receipt wraps it. |
| `TurnCancelReceipt` | *unchanged* | Acknowledges one addressed cancellation gate. |
| `TurnInputAcceptanceReceipt` | *unchanged* | Durable acceptance evidence for an ingress caller. |
| `TurnOutcome` | *unchanged* | Closed enum: finished, frame switch, stopped. |
| `TurnResult` | `TurnReport` | Aggregate over one turn: the `TurnOutcome` plus per-item llm calls, tool calls, activities, issues, and the usage ledger. **CONTESTED** — a record noun (`TurnRecord`) would read better as the daily return type, at the cost of leaving the four nouns for the facade's most prominent answer. |
| `TurnResultSummary` | `TurnHookReport` | The plugin-hook mirror of `TurnReport` — the same aggregate over one turn, named for where it is delivered. |
| `VacuumReport` | *unchanged* | Maintenance aggregate of physically removed rows. |
| `WakeDeliveryDriveReport` | *unchanged* | Drive aggregate of per-category counts. |
