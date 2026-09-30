# One meaning per outcome-type suffix

## Status

Accepted.

## Context

A host needs to distinguish acknowledgement, completion, aggregate detail and
live observation from a type's name. Using the same suffix for inputs,
policies and outputs makes that distinction depend on reading every field.
`Result` also has a precise Rust meaning that domain nouns must not obscure.

## Decision

An outcome type's final noun states what kind of answer it carries. Each of the
four permitted suffixes has one role:

| Suffix | Meaning |
| --- | --- |
| `Receipt` | Durable acknowledgement that a request is accepted or recorded. It does not promise that the requested work completes. |
| `Outcome` | The terminal answer of one operation. Usually a closed enum; a struct is appropriate when that operation has one terminal answer shape. |
| `Report` | An aggregate over items, including maintenance counts, batch deliveries and a turn's calls or usage. |
| `Status` | A point-in-time observation of work that can remain in flight. Its state set can include terminal observations. |

The operation an outcome answers is explicit. A tool body's return and a whole
tool call's settlement are different operations. An observation operation can
finish while the observed root is parked; ending that observation does not
make the root terminal.

`Result` is reserved for `std::Result` and genuine aliases.
`Disposition` and `Summary` are not domain-type suffixes. A caller's choice is
a `Policy`, a producer's declaration is a `Contract`, and a read projection is
a domain noun or `View`. The four answer nouns do not apply to every type.

### What this decision does not touch

The rule governs type identifiers. Serde fields and variants are their own wire
contract. For example, `TriggerMutationReceipt::disposition` carries a
`TriggerMutationOutcome` without changing the serialized field name.
`LlmContentBlock::ToolResult` identifies a message-block variant serialized as
`tool_result`; it is not a domain type named `ToolResult`.

Module paths such as `lash::remote::turn_result` and local variable or test
function names are not outcome-type identifiers. A naming rule cannot authorize
changing serialized fields, identity tags or durable format versions.

The enforced inventory includes public type declarations and names bound by
`pub use`, including aliases, throughout workspace Rust sources in `crates`,
`examples` and `runbooks`. A glob re-export's declarations are checked at their
definition. The gate excludes separate tests, `cfg(test)` support and the named
vendored Restate protocol output whose type vocabulary belongs to that protocol.
Private implementation names follow the same naming roles when changed.

### Two near-neighbours, kept apart on purpose

`ToolOutcome` answers a tool body's return, `Done` or `Pending`.
`ToolCallOutcome` answers the call's settlement, `Success`, `Failure` or
`Cancelled`. Both are outcomes of their own operation.

`ToolRetryPolicy` is the configured choice. `ToolRetryStatus` observes retry
progress, including exhaustion. A policy is not a status merely because both
refer to retries.

Likewise, an addressed cancellation receipt binds a target to its closed cancel
outcome. A batch report collects per-delivery receipts rather than calling one
delivery a report.

## Alternatives considered

Documenting inconsistent suffixes preserves a vocabulary that fails to predict
the type's role. One shared rule makes adjacent exports readable without a
per-domain glossary.

Incremental dual naming leaves a host choosing between competing names for the
same role. Compatibility aliases retain that ambiguity and are not part of
this contract.

`Summary` for projections says less than naming what the projection shows.
`View` or the domain noun states that information directly.

Merging a receipt and its outcome because their names look similar erases the
addressed acknowledgement or the reusable closed state. Any merge requires its
own semantic reason; vocabulary alone supplies none.

## Consequences

New public type names obey the roles. A domain type ending in `Result`,
`Disposition` or `Summary`, or a single-item answer ending in `Report`, is a
review finding. Identifier-only changes do not themselves change payloads or
justify format-version bumps.

## Appendix: reclassification table

These current types demonstrate the roles rather than recording a rename list.

| Current type | Role |
| --- | --- |
| `TurnInputAcceptanceReceipt` | Accepted input identity before execution. |
| `RuntimeCommitReceipt` | Durable acknowledgement of a committed operation. |
| `TriggerDeliveryEmitReceipt` | One addressed delivery and its emission outcome. |
| `TriggerDeliveryEmitOutcome` | Closed answer for that delivery's emission. |
| `TriggerEmitReport` | Aggregate of delivery receipts for one occurrence. |
| `TriggerMutationOutcome` | Closed answer for one subscription mutation. |
| `TriggerMutationReceipt` | Recorded mutation acknowledgement, carrying its outcome. |
| `ProcessRegistrationOutcome` | Registration's answer, including an existing registration. |
| `ToolOutcome` | The tool body's return. |
| `ToolCallOutcome` | The call's settlement. |
| `ToolRetryStatus` | Retry progress observation. |
| `LoserPolicy` | Caller-chosen group policy, not an answer. |
| `RecoveryContract` | Producer declaration, not an answer. |
| `ProcessHandleView` | Read projection of the process handle. |
| `TurnExecutionMetrics` | Measured execution facts. |
| `TurnReport` | Aggregate of the turn outcome, calls, errors and usage. |
| `MaintenanceResult<R>` | A genuine fallible-return alias. |

## Executable evidence

- [Suffix gate](../../scripts/check_outcome_suffixes.py#L1) defines the scan,
  public aliases, exclusions and the four allowed Result aliases: `Result`,
  `MaintenanceResult`, `TriggerEffectResult` and
  `TriggerOccurrenceReclamationResult`.
- [Role witnesses](../../scripts/test_identity_adr_claims.py#L23) pin delivery
  receipt/report nesting, registration outcomes and retry status.
- [Tool-body answer](../../crates/lash-core-execution/src/tool_result.rs#L382),
  [call outcome](../../crates/lash-sansio/src/tool_output.rs#L455) and
  [retry status](../../crates/lash-sansio/src/tool_output.rs#L893) keep the
  neighbouring roles separate.
- [Turn report](../../crates/lash/src/turn.rs#L158) and
  [remote status](../../crates/lash-remote-protocol/src/turn_result.rs#L217)
  show aggregate and observation roles, including parked and stalled work.
