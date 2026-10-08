# Final-output attribution and presentation are host policy

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

The same split holds for every output: lash exposes facts, never their
presentation or a judgment of their quality (FIG-5430). A turn report carries
its typed `TurnOutcome` (the model's assistant text unmodified, a final or
tool value, or a typed stop) and its typed issues; it carries no sanitized
copy of its text and no usability classification. Committed history is
typed entries a host renders ([ADR 0129](0129-committed-history-is-typed-facts-a-host-renders.md));
lash joins no text, pretty-prints no value and names no status in words.

## Consequences

A model badge is a host projection, not runtime execution evidence. A single
runtime rollup is rejected because a visible output can correspond to zero,
one or several calls, and the per-call records already contain the facts.

A trimmed reply, an "empty output" badge or a traceback warning is likewise
host policy over the outcome and its issues.

## Implementation

[Turn report vocabulary](../../crates/lash-core-execution/src/runtime/vocabulary.rs)
carries `llm_calls` and the outcome; [call records](../../crates/lash-sansio/src/llm/types.rs)
carry provider evidence and attempt identity.
