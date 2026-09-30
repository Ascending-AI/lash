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

An interrupted attempt with unreported usage contributes a
`LedgerUsageOutcome::Unreported` entry even when its token counters are zero.
Session usage reports expose the outstanding unreported count beside totals.
Host-invoked reconciliation appends `Reconciled` correction rows attributed to
the call and attempt; it does not rewrite the original observation. A provider
lookup with absent or null token counts leaves the hole open. Explicitly
reported zero closes it.

Turn, trace and remote projections carry execution evidence and usage
dispositions. Higher-level model attribution remains host policy under
ADR 0033.

## Consequences

A host can distinguish observed zero cost from missing accounting and can
inspect served-model drift across attempts. An adapter that cannot report a
field leaves it absent. Echoing model intent as execution evidence and
zero-filling missing usage are rejected because they manufacture facts.

## Implementation

- [Evidence and attempt usage types](../../crates/lash-sansio/src/llm/types.rs).
- [Usage ledger and correction folding](../../crates/lash-core-store/src/usage.rs).
- [Host-invoked reconciliation](../../crates/lash-core/src/runtime/session_api.rs) and [OpenRouter accounting lookup](../../crates/lash-provider-openai/src/openrouter.rs).

## Model usage accounting

The ledger identity is effect-keyed:
`(owner, effect, call_ordinal, provider_attempt, kind)`. `LlmCallId` is not
unique per session (every session direct call is `"{session}:direct"`), so
it rides on the fact as attribution only. An unreported attempt keeps this
ADR's typed meaning and is an `unreported` fact; `UnreportedByProvider`
records nothing. A correction is its own fact kind, appended by
`append_usage_corrections` ([ADR 0125](0125-model-usage-is-engine-owned-accounting-delivered-per-call.md)).
