# 0121: Host generation settings are sent or refused

## Status

Accepted.

## Context

A host's generation settings are execution intent. An adapter that silently
drops a setting can issue a call with a different cost or sampling contract.
A receipt after the call cannot prevent that mismatch. Model capability and
wire representability therefore need one exact resolution before transport.

## Decision

Every setting a host gives lash reaches the provider request in exactly one of
two ways. It is sent. Or the call is refused before any I/O, with a typed,
non-retryable failure. Nothing is dropped and nothing is remapped. Two
deliberate non-send dispositions remain, and the per-call `GenerationReceipt`
names both:

- an output-token cap reduced to the recorded profile's `output_tokens.capacity`
  (`ClampedToCapacity`);
- stop sequences a protocol suppresses because its grammar owns the response
  boundary (`SuppressedProtocolOwned`).

`expose_thinking` is local-publication intent. Every adapter publishes the
reasoning a provider streams when it is set. A wire that needs a flag to
produce that reasoning is sent one: Responses and Codex `reasoning.summary`,
Anthropic `display` inside active thinking, Google `includeThoughts`. Chat
Completions has no such flag, so nothing is sent there, and the host's intent
is still fully honored locally. The receipt splits the two halves into
`thinking_summary` and `thinking_visibility`.

**Output-token facts have one recorded home.** `OutputTokenLimits` holds
optional non-zero capacity and default cap. Construction and decoding reject
a default above capacity. Requests carry the full `LlmProfileConfig`; a
session-owned direct completion takes it from its owner. Generation resolution
selects the request cap or recorded default and bounds it against capacity
once. Requests and recorded calls retain the original intent. The adapter's receipt
reports `ClampedToCapacity` only when that bounded cap reached the wire.

**Reasoning resolves once.** `LlmProfileCapability::reasoning_intent` resolves the
session's recorded `ReasoningSelection` against the recorded capability into
`Option<ReasoningIntent>`, where `ReasoningIntent` is
`Effort(String) | Budget(u32) | Off`. `ProviderDefault` resolves to `None` and
sends nothing. A budget map missing an advertised effort is
`malformed_capability`, never an omission. `validate_selection`, run at the
runtime seams, is that same resolution. It never rewrites the selection.

`ReasoningCapability` is `{ efforts, encoding, disable, mandatory }`. `disable`
is a plain flag saying the route accepts an explicit off. The route's dialect
owns the wire form of off. There is no default effort in capability data: a
selection left unset records `ProviderDefault`, which sends nothing.

**Each wire maps the intent in one plain function:**

| Wire | Effort | Budget | Off |
|---|---|---|---|
| Anthropic Messages | adaptive `thinking` + `output_config.effort` | `thinking.budget_tokens`, below `max_tokens` | `thinking: disabled` |
| Google Legacy | `thinkingLevel` | `thinkingBudget` | `thinkingBudget: 0` |
| Google Gemini3 | `thinkingLevel` | `thinkingBudget` | refused |
| Google ClaudeOnVertex | refused | `thinkingBudget`, below the cap | refused |
| OpenAI-compatible, `OpenAi` dialect | Chat `reasoning_effort`; Responses `reasoning.effort` | refused | effort `none` |
| OpenAI-compatible, `OpenRouter` dialect | `reasoning.effort` | `reasoning.max_tokens` | `reasoning.enabled: false` |
| Codex | Responses in the `OpenAi` dialect | refused | effort `none` |

Off also requires the capability's `disable`.

**OpenAI-compatible routes carry a closed dialect.**
`OpenAiCompat.reasoning: Option<OpenAiReasoningDialect>` is
`OpenAi | OpenRouter`, and the host or a preset sets it:

- `OpenAiProvider` (Responses) and the `OpenAiCompat::openai_chat()` preset set
  `OpenAi`;
- `OpenAiCompat::openrouter()` sets `OpenRouter`;
- `OpenAiCompat::local()` sets none.

A route with no dialect refuses an explicit selection with
`reasoning_encoding_unrepresentable`, and `ProviderDefault` sends nothing. No
URL selects a dialect. Gateways can ignore wrong-shape fields, so guessing a
dialect can move an unsupported selection past local validation without
honoring it.

A closed, serializable dialect travels with the host's route configuration.
A custom trait object cannot supply the same durable and remote contract.

**Generation options resolve once, against the wire.**
`resolve_generation_policy` takes the request, the provider kind and a
`GenerationWire` in which each adapter states what its wire carries for this
call. It refuses everything else before the adapter does any I/O:

- **Cap.** The effective cap is the request's, else the recorded model's
  `LlmProfileLimits.output_tokens.default_cap`. With neither set, an optional-cap wire
  sends none, and Anthropic refuses with `output_token_cap_required`. Codex
  and `max_tokens_field: Omit` endpoints refuse any cap.
- **Temperature.** A wire without a temperature field refuses one. A model
  whose capability pins sampling refuses one on every adapter. Active
  Anthropic thinking, including Claude on Vertex, refuses one while an effort
  or budget is selected.
- **Seed and stop sequences** are refused on wires with no field: Anthropic
  and Responses (seed), Responses and Codex (stop), and Codex for both.
- **`parallel_tool_calls`** is a typed `Option<bool>` on `GenerationOptions`
  and replaces implicit adapter choices. It is sent on Chat (with tools), on
  Responses and Codex, and on Anthropic as
  `tool_choice.disable_parallel_tool_use` (with tools). Google has no such
  control and refuses it.

Refusals carry `unsupported_generation_option`, `output_token_cap_required`,
`reasoning_encoding_unrepresentable`, `reasoning_budget_exceeds_output_cap` or
the effort-validation codes. All are `Forbidden` for retry.

Lash adds only mechanics to the wire. `store: false` and
`include: ["reasoning.encrypted_content"]` stay, because stateless replay
needs them. Verbosity and parallel-call defaults are left to the provider
when the host sets none.

**Validation precedes I/O.** Codex resolves every setting before its
credential manager may refresh a token. Google resolves before the credential
refresh, the project lookup and any attachment upload.

**The receipt joins provenance with emission evidence.** Resolution supplies
what the host asked for. Each adapter reports, from the branch that wrote it,
what it put on the wire, through `ResolvedGenerationPolicy::receipt`. The
receipt is not read back off the body. `Applied` means lash sent the value,
never that the provider complied. The receipt includes `reasoning`,
`parallel_tool_calls`, `thinking_summary` and `thinking_visibility`.

`OmittedUnsupported` applies only to the `cache` row. That row reports
prompt-cache breakpoints placed by the protocol, not a host setting.

**Recorded policy shapes are strict.** `ReasoningCapability` and its
remote mirror deny unknown fields, so a persisted `default_effort`, `aliases`
or encoded `disable` fails to load rather than being ignored. An `OpenAiCompat`
naming `reasoning_format` is refused the same way. The receipt decoder
refuses an `omitted_sampling_pinned` outcome.

## Consequences

- A mixed-model session that sets a session-wide temperature or seed must
  clear it for models or wires that cannot carry it
  (`GenerationOverlay::Replace`). A receipt cannot un-send a call.
- Hosts on Anthropic set `LlmProfileLimits.output_tokens.default_cap` or a request cap.
- Hosts choose a preset or set
  `OpenAiCompat.reasoning`.
- Replay-route ownership is exact. Opaque reasoning, tool-call and
  response-item state is reusable only on the exact minting route, and dialect
  equality never makes two replay routes equal.

## Implementation

- `crates/lash-sansio/src/llm/capability.rs:434` resolves reasoning intent.
- `crates/lash-core-llm/src/provider/options.rs:299` resolves generation
  options; `:389` joins requested provenance to adapter emission evidence.
- `crates/lash-provider-google/src/request.rs:369` declares its wire and
  maps the selected reasoning dialect.
- `crates/lash-provider-anthropic/src/request.rs:434` declares its wire;
  `crates/lash-provider-anthropic/src/policy.rs:27` maps reasoning.
- `crates/lash-provider-openai/src/codex.rs:224` validates before credential
  access; `crates/lash-provider-openai/src/responses.rs:43` builds the
  Responses request from resolved policy.
- `crates/lash-core-llm/src/provider/tests/generation_policy_tests.rs` and
  the providers' generation tests pin refusal and emission.

The pre-1.0 freeze changes these shapes in place. Exact effort matching and
refusal avoid guessing a substitute sampling contract. Provider defaults stay
provider-owned when the host omits a setting.
