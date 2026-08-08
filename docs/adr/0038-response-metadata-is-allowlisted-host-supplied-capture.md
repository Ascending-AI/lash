# Response metadata is allowlisted host-supplied capture

The typed success-result boundary was lossy. OpenAI-compatible drivers received response headers
but retained only `x-request-id`, while unmodeled top-level body members disappeared when the
wire response became an `LlmResponse`. A host that needed a provider receipt therefore had to
decorate the transport, observe the response out of band, and correlate that observation back to
the completed call. `RemoteProviderMetadata.data` did not close the gap: outbound conversions
always created it empty and inbound conversions discarded it.

We decided that **wire metadata capture is explicit, allowlisted, and supplied by the host**.
`LlmResponse.response_metadata` is a map of raw JSON values. Shared `ProviderOptions` may
allowlist case-insensitive response header names and JSON pointers into response bodies for every
first-party provider.
Captured headers use `header:<lowercased-name>` keys; captured body values use
`body:<json-pointer>` keys. Buffered responses probe the final body, while streaming responses
probe every JSON SSE event and retain the last observed value for a pointer. Both allowlists are
empty by default. The shared LLM transport captures response headers, buffered bodies, and SSE
events before the provider-specific parser sees them, so OpenAI, Anthropic, and Google have the
same capture semantics.

Lash provides only the capture mechanism. The host owns the meaning of every captured value,
consistent with the host-policy boundary in ADR 0033: core contains no gateway-specific keys or
parsers and no typed cost field. In particular, there is no dump-all mode. Durable results must
not silently absorb cookies, authentication-adjacent headers, unbounded diagnostic values, or
high-cardinality wire data. A value enters `response_metadata` only because the host named its
header or JSON pointer on that endpoint.

Capture follows the attempt result. A successful response carries observations from its final
assembled response path, and a failed streaming attempt's partial response carries observations
accumulated before failure, just as it carries provider request identity and partial usage. HTTP
error-status responses remain unchanged because their error envelope already carries the full
response headers. The remote protocol now transports the map through
`RemoteProviderMetadata.data` in both directions using deterministic `BTreeMap` ordering instead
of manufacturing an empty map and discarding it on return.

We rejected adding a separate correlation token for transport decorators. Once the observation
travels in-band on the typed response or partial response, a second correlation mechanism is
obsolete and would preserve the workaround rather than the missing seam. We also rejected
duplicating the allowlist fields and capture loops in every provider config. Unlike stream
termination, capture has no wire dialect: `ProviderOptions` is the host interface and the shared
transport is the one honest implementation seam.
