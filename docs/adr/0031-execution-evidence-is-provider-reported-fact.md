# Execution evidence is provider-reported fact

## Context

Requested and resolved model settings express intent. They cannot prove which
model a provider served or what usage it reported. Hosts need those facts even
when a call fails or a stream ends before its terminal event.

## Decision

`ExecutionEvidence` carries only provider-reported facts: served model,
response id, transport request id, reasoning output tokens and provider finish
reason. Its optional collection-interruption field describes deliberate
preemption of evidence collection. Adapters never fill execution facts from
requested or resolved intent. Absence means unreported; `Some(0)` and `None`
remain distinct.

Each response carries its evidence. Each transport attempt also records the
facts observed before it ends, including failed, aborted and interrupted
attempts. ADR 0032 defines the call ledger that carries those records on both
success and failure. A call's optional label is open host or plugin vocabulary,
not a core enumeration of reasons for model work.

`AttemptUsageOutcome` distinguishes `Reported`, `UnreportedByProvider`,
`UnreportedAfterAbort` and `UnreportedAfterFailure`. It derives from observed
usage and the attempt outcome. Reported usage may be partial; it is still an
observation rather than a guessed final count.

An interrupted attempt retains its typed usage disposition and any partial
usage in the call result. A host settles receipts and retains outstanding
billing evidence at the provider boundary under ADR 0127.

Turn and trace projections carry execution evidence and usage
dispositions. Higher-level model attribution remains host policy under
ADR 0033.

## Consequences

A host can distinguish observed zero cost from missing accounting and can
inspect served-model drift across attempts. An adapter that cannot report a
field leaves it absent. Echoing model intent as execution evidence and
zero-filling missing usage are rejected because they manufacture facts.

## Implementation

- [Evidence and attempt usage types](../../crates/lash-sansio/src/llm/types.rs).

## Model usage

Usage is data on the model call's recorded result. Hosts meter spend at the
`Provider` seam under [ADR 0127](0127-usage-is-result-data-hosts-meter-spend.md).
Lash has no accounting ledger or delivery dependency.
