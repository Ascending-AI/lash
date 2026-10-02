# Stream termination is explicit dialect policy

## Context

A clean transport EOF does not prove a complete model response. A stream can
lose its terminal event while retaining text, partial tool arguments and usage.
Treating those fragments as success can execute an incomplete tool call.

## Decision

Completion requires dialect-specific terminal evidence unless the host
explicitly selects `StreamTermination::EofTolerated`. The alternative is
`RequireTerminalEvidence`. `LlmProfileCapability.stream_termination` overrides the
route default. OpenAI-compatible endpoint defaults live in `OpenAiCompat`;
Anthropic and Google expose the same policy in provider configuration.

Chat Completions requires a nonempty `finish_reason`. Responses requires a
terminal response event. Anthropic requires `message_stop` after
`message_start`. Google defaults to EOF tolerance. The OpenRouter-compatible
preset requires terminal evidence. Neither a URL, a model name nor `[DONE]`
substitutes for the selected dialect's evidence.

Missing required evidence is a `ProviderFailureKind::Stream` failure, subject
to retry and host charge-safety policy. Its partial response retains accumulated
output, usage, provider usage, execution evidence and allowlisted metadata.
That response crosses the effect boundary for accounting and diagnosis; the
protocol never receives it as completed output. The attempt ledger records the
observed facts on an `Interrupted` attempt. A whole-call retry resets provisional
output and usage before collecting the next attempt (ADR 0040).

Explicit cancellation is distinct from truncation. A protocol-owned abort can
still drain provider usage before sealing its attempt. The host selects
`abort_drain_grace`, whose default is two seconds. Usage received in that
interval remains provider-reported; missing usage becomes
`UnreportedAfterAbort` (ADR 0031).

A provider may implement post-hoc `reconcile_usage`. OpenRouter's configured
lookup is bounded. The runtime invokes reconciliation only when the host calls
`reconcile_unreported_usage`, not as an automatic background policy.

## Consequences

A host using an EOF-terminated compatible route must state that policy.
Heuristic fallback to success is rejected because it hides truncation. Abort
drain timing and later accounting remain host choices, while evidence records
what the provider actually reports.

## Implementation

- [OpenAI stream validation](../../crates/lash-provider-openai/src/driver.rs) and [Responses collection](../../crates/lash-provider-openai/src/codex/streaming.rs).
- [Anthropic completion validation](../../crates/lash-provider-anthropic/src/provider.rs) and [Google defaults](../../crates/lash-provider-google/src/config.rs).
- [Abort drain](../../crates/lash-core/src/runtime/turn_driver/streaming.rs) and [usage reconciliation](../../crates/lash-provider-openai/src/openrouter.rs).
