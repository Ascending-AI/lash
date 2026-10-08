# Stream termination is explicit dialect policy

## Context

A clean transport EOF does not prove a complete model response. A stream can
lose its terminal event while retaining text, partial tool arguments and usage.
Treating those fragments as success can execute an incomplete tool call.

## Decision

`StreamTermination::EofTolerated` is the policy when neither the model nor
route selects another. A clean EOF may complete accumulated output under that
policy; it does not make malformed tool arguments or an empty response valid.
`LlmProfileCapability.stream_termination` overrides the route policy.
OpenAI-compatible endpoint policy lives in `OpenAiCompat`; Anthropic and Google
expose the same choice in provider configuration.

`StreamTermination::RequireTerminalEvidence` is the explicit strict choice.
Under it, Chat Completions requires a nonempty `finish_reason`, Responses a
terminal response event, and Anthropic `message_stop` after `message_start`.
The OpenRouter-compatible preset explicitly selects this strict policy.
Neither a URL, a model name nor `[DONE]` substitutes for the selected dialect's
required evidence.

Missing required evidence is a `ProviderFailureKind::Stream` failure, subject
to retry and host charge-safety policy. Its partial response retains accumulated
output, usage, provider usage, execution evidence and allowlisted metadata.
That response crosses the effect boundary for accounting and diagnosis; the
protocol never receives it as completed output. The attempt ledger records the
observed facts on an `Interrupted` attempt. A whole-call retry resets provisional
output and usage before collecting the next attempt (ADR 0040).

Explicit cancellation is distinct from truncation. A protocol-owned abort can
still drain provider usage before sealing its attempt. The host selects
the stop grace of its execution budgets (`ExecutionBudgets::stop_grace`),
which is two seconds in `ExecutionBudgets::recommended()`. Usage received in that
interval remains provider-reported; missing usage becomes
`UnreportedAfterAbort` (ADR 0031).

A host may reconcile receipts after a call through its provider decorator
(ADR 0127). Lash supplies the captured response metadata and attempt history,
and provides no reconciliation API.

## Consequences

A host that needs truncation detection selects `RequireTerminalEvidence`.
EOF tolerance follows the selected policy; missing evidence never silently
relaxes a strict route. Abort
drain timing and later accounting remain host choices, while evidence records
what the provider actually reports.

## Implementation

- [Termination policy](../../crates/lash-sansio/src/llm/capability.rs) and [OpenAI route resolution](../../crates/lash-provider-openai/src/config.rs).
- [OpenAI stream validation](../../crates/lash-provider-openai/src/driver.rs) and [Responses collection](../../crates/lash-provider-openai/src/codex/streaming.rs).
- [Anthropic completion validation](../../crates/lash-provider-anthropic/src/provider.rs) and [Google defaults](../../crates/lash-provider-google/src/config.rs).
- [Abort drain](../../crates/lash-core/src/runtime/turn_driver/streaming.rs).
