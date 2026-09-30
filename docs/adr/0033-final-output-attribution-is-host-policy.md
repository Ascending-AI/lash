# Final-output attribution is host policy

## Context

A turn can contain several model calls, transport fallbacks and child sessions
with different models. A protocol final value or tool result can become output
without any assistant-text call producing it. One producing-model field would
discard facts or select an arbitrary call.

## Decision

Lash exposes execution facts at call granularity: provider-reported
`ExecutionEvidence` and the attempt ledger, aggregated as `LlmCallRecord`
entries on the turn report (ADRs 0031 and 0032). Lash computes no turn-level or
session-level model attribution and no final-output provenance tag.

Hosts compose any higher-level attribution from those records and their
product's meaning of an output or message. A host may display resolved intent,
a set of contributing served models or a selected call, but that choice is
host policy. Child calls stay on their child results rather than being
silently flattened into the parent's ledger.

## Consequences

A model badge is a host projection, not runtime execution evidence. A single
runtime rollup is rejected because a visible output can correspond to zero,
one or several calls, and the per-call records already contain the facts.

## Implementation

[Turn report vocabulary](../../crates/lash-core-execution/src/runtime/vocabulary.rs)
carries `llm_calls`; [call records](../../crates/lash-sansio/src/llm/types.rs)
carry provider evidence and attempt identity.
