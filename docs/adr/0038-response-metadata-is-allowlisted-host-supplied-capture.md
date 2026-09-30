# Response metadata is allowlisted host-supplied capture

## Context

A typed response omits unmodeled provider headers and body fields. Hosts that
need selected receipts should receive them with the attempt result rather
than correlate a separate transport observation.

## Decision

`LlmResponse.response_metadata` is a map of raw JSON values. Shared
`ProviderOptions` supplies two allowlists: case-insensitive response header
names and JSON pointers into response bodies. Both are empty by default.
Captured headers use `header:<lowercased-name>` keys; body values use
`body:<json-pointer>` keys.

The shared transport captures headers, buffered bodies and JSON SSE events
before provider-specific parsing. Streaming capture retains the last observed
value at each pointer. An absent pointer adds nothing. Invalid or non-JSON
events do not change response parsing semantics. OpenAI, Anthropic and Google
use this common capture mechanism.

Successful responses carry observations from their response path. Failed
streaming attempts carry observations accumulated before failure on their
partial responses. Error-status transport envelopes carry response headers.
Remote conversion preserves the metadata map through
`RemoteProviderMetadata.data` in both directions using `BTreeMap` ordering.

Lash owns capture, not the meaning or suitability of captured values. The
host chooses every header and pointer. Core has no gateway-specific metadata
parser, typed cost field or dump-all mode. Capture is an explicit host
configuration choice, not a Lash security policy.

## Consequences

Hosts receive selected wire observations in-band on the response or partial
response. A separate decorator correlation token is rejected because the typed
result already carries the observation. Provider-specific copies of capture
configuration and loops are rejected because capture has no dialect-dependent
semantics.

## Implementation

[Shared capture](../../crates/lash-llm-transport/src/response_metadata.rs),
[response types](../../crates/lash-sansio/src/llm/types.rs) and
[remote conversion](../../crates/lash-remote-protocol/src/core_conversions/llm.rs)
define the mechanism and its result path.
