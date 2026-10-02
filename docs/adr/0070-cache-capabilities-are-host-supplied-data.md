# Cache capabilities are host-supplied data

## Context

OpenAI-compatible endpoints and model routes can share wire behavior while using
different URLs and model names. The host knows that behavior. URL and name
inference cannot express the endpoint's contract reliably.

## Decision

Cache-control dialect is model capability data. `LlmProfileCapability.cache_control`
is `Option<CacheControlDialect>`. `None` emits no Chat Completions
`cache_control`. `Anthropic` places cache control on initial instructions,
tools, and explicit conversation breakpoints, and supports the one-hour TTL.
`Gemini` emits one ephemeral breakpoint without a TTL, choosing the last
explicit breakpoint or a trailing text block. The capability travels with the
model through turn, direct, and remote requests under ADR 0026.

Session affinity is endpoint compatibility data.
`OpenAiCompat.cache_session_affinity` defaults to disabled. Enabling it emits
the bounded body `session_id` and call-specific `x-client-request-id` for the
configured endpoint. `OpenAiCompat::openrouter()` explicitly enables affinity
and selects the OpenRouter reasoning dialect. A host can use the preset for a
proxy with the same contract.

General compatibility fields obey the same rule. `OpenAiCompat::local()` sets
`request_fields`, `store`, and `streaming_usage` to false. Without an explicit
setting those fields default to true, including for a localhost URL. No URL or
model name selects a compatibility preset.

`OpenAiProvider` explicitly selects the OpenAI reasoning dialect and prompt
cache fields. `OpenAiCompat::openai_chat()` explicitly selects the OpenAI Chat
Completions dialect. Reasoning controls follow
[ADR 0121](0121-host-generation-settings-are-sent-or-refused.md).

## Alternatives considered

Inferring behavior from a canonical endpoint URL fails for compatible proxies.
Inferring a cache dialect from a model name fails for renamed or differently
routed models. Neither inference is an acceptable fallback for absent data.

## Consequences

Hosts declare the cache dialect for each supported model route and compatibility
for each endpoint. Omission means the capability is unsupported. An endpoint
with a stricter request contract can reject undeclared fields; the host selects
the appropriate preset or individual compatibility fields.

## Code references

- `crates/lash-provider-openai/src/config.rs:127-164,202-239` defines presets and defaults.
- `crates/lash-provider-openai/src/chat.rs:336-406` places model-selected cache control.
- `crates/lash-provider-openai/src/provider.rs:56-66` configures direct OpenAI explicitly.
- `crates/lash-provider-openai/src/driver.rs:118-142` applies session-affinity headers.
