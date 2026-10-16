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

Explicit cancellation is distinct from truncation. A protocol-owned abort ends
the protocol's stream, not the provider call: the accepted cell runs at once,
and the call's tail is read to the provider's terminal event off the turn's
path, its content discarded (FIG-6433). The provider future, and any host
decorator around it, runs to its end, and the call's record, the turn's usage
totals and its span carry the usage the provider reported. The drain is bounded
by the call's pinned deadline; the turn waits for it before it closes or parks.
Usage that never arrives, because the deadline passed, the provider failed or
the turn's immediate cancel stopped the tail, is unknown, never zero: the
attempt's usage is absent (`UnreportedAfterAbort` or `UnreportedAfterFailure`,
ADR 0031), the turn's `LlmUsage::unknown_calls` counts the call, and the span
carries `lash.usage.unknown`.

A host may reconcile receipts after a call through its provider decorator
(ADR 0127). Lash supplies the captured response metadata and attempt history,
and provides no reconciliation API.

## Consequences

A host that needs truncation detection selects `RequireTerminalEvidence`.
EOF tolerance follows the selected policy; missing evidence never silently
relaxes a strict route. Abort
drain is bounded by the call's deadline, later accounting remains a host
choice, and evidence records what the provider actually reports.

## Implementation

- [Termination policy](../../crates/lash-sansio/src/llm/capability.rs) and [OpenAI route resolution](../../crates/lash-provider-openai/src/config.rs).
- [OpenAI stream validation](../../crates/lash-provider-openai/src/driver.rs) and [Responses collection](../../crates/lash-provider-openai/src/codex/streaming.rs).
- [Anthropic completion validation](../../crates/lash-provider-anthropic/src/provider.rs) and [Google defaults](../../crates/lash-provider-google/src/config.rs).
- [Abort drain](../../crates/lash-core/src/runtime/turn_driver/streaming/drain.rs).
