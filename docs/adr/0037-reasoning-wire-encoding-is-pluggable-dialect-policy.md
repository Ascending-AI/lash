# Reasoning wire encoding is pluggable dialect policy

OpenAI-compatible Chat Completions gateways disagree on how reasoning intent is encoded, and
many silently ignore fields from another dialect. Opper's compatibility endpoint, for example,
requires top-level `reasoning_effort: "minimal"`; the nested `reasoning: { "effort":
"minimal" }` form was silently ignored in a live probe and incurred 128 reasoning tokens where
the top-level form incurred zero. OpenRouter requires the nested object, while other gateways
use shapes such as `thinking: { "type": ... }` or `enable_thinking`. This space is too broad and
fast-moving for a closed provider enum.

We decided that **reasoning wire encoding is an explicit, host-supplied dialect policy**.
`ReasoningWireFormat` carries the selected encoder, with built-ins for `none`, `openai`, and
`openrouter`; hosts implement `ReasoningWireEncoder` and attach a custom format for every other
dialect. The provider first resolves the host-supplied `ReasoningSelection` and
`ReasoningCapability` into a dialect-independent `ReasoningWireIntent`, then asks the selected
dialect to mutate the outgoing request body. A dialect that cannot represent an intent returns
a deterministic, non-retryable `reasoning_encoding_unrepresentable` error. It never silently
omits the selection or falls back to another shape.

Defaults remain explicit dialect facts. Direct OpenAI Responses uses the `openai` encoder;
unknown OpenAI-compatible endpoints use `none`, which transmits no reasoning selection. Hosts
must only declare reasoning vocabularies on routes where they also select a verified dialect.
There is no new base-URL guessing, and the existing direct-OpenAI Responses default is the only
URL-derived choice retained. `OpenAiCompat::openrouter()` continues to be an explicit preset,
not detection.

Built-in names serialize as `none`, `openai`, and `openrouter`. Custom dialects are
programmatic-only: their names serialize for diagnostics and configuration identity, but
deserializing any unknown name fails loudly and explains that a host must attach the encoder
programmatically. A name is the encoder's equality identity, so hosts must assign unique names.

This follows ADR 0026 and ADR 0034: model capability is host-supplied data and providers consume
it rather than rediscovering it. It also follows ADR 0036: wire-protocol differences are
explicit dialect policy, not inference or optimistic fallback.

This dialect choice is separate from LLM Provider replay ownership. A host may select the same
reasoning dialect for several endpoints, but opaque reasoning, tool-call, and response-item state
is reusable only on the exact minting route: provider kind, normalized configured endpoint, and
model id. Both decisions are host-selected; dialect equality never makes two replay routes equal.
