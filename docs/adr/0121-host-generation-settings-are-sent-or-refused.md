# 0121: Host generation settings are sent or refused

## Status

Accepted 2026-09-29 (FIG-4120). Supersedes
[ADR 0072](0072-reasoning-wire-encoding-is-pluggable-dialect-policy.md).
Amends [ADR 0026](0026-model-capability-is-host-supplied-data.md) (the
capability shape and where defaults live),
[ADR 0070](0070-cache-capabilities-are-host-supplied-data.md) (no URL-derived
choice survives) and
[ADR 0074](0074-generation-intent-is-session-policy-and-its-fate-is-reported.md)
(refusal instead of remapping, shared pinning, the new receipt rows).

The rulings on audit G9 (Sam, 2026-09-29) bind this decision:

1. **Q1: refuse.** A setting the call's model or wire cannot carry is refused,
   for route settings and session-wide `GenerationOptions` alike. Nothing is
   remapped.
2. **Q2: no invented defaults.** Lash's 32,768-token default cap is gone.
   With no cap set, none is sent; a wire that requires one refuses the call.
3. **Effort matches exactly.** No clamping to a "nearest" effort, no aliases,
   no case folding.
4. **Whole-hog and the version freeze.** The old path is deleted with no shims
   or dual paths, and shapes change in place.

## Context

Lash accepted host settings it then dropped without a word. A validated
`Effort("high")` on any OpenAI-compatible route whose reasoning format was the
default `none` vanished from the request. That covered direct OpenAI Chat
Completions, Azure and every compatible gateway. A `Disabled` selection with
the `Omit` encoding sent nothing. Google sent `temperature: 0` when the host
set none and ignored pinned sampling. Anthropic and Google always sent a
32,768-token cap nobody asked for. Codex dropped every sampling control and
sent OpenRouter-shaped reasoning fields on the Responses wire. Four providers
each resolved `ReasoningSelection × ReasoningCapability` their own way, and
gave the same `Native` and `ToggleFalse` tokens different meanings.

ADR 0074 made these omissions observable after the call. A receipt arrives too
late, though, to stop an uncapped or mis-sampled call.

## Decision

Every setting a host gives lash reaches the provider request in exactly one of
two ways. It is sent. Or the call is refused before any I/O, with a typed,
non-retryable failure. Nothing is dropped and nothing is remapped. Two
deliberate non-send dispositions remain, and the per-call `GenerationReceipt`
names both:

- an output-token cap reduced to the model's `output_token_capacity`
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

**Reasoning resolves once.** `ModelCapability::reasoning_intent` resolves the
model spec's `ReasoningSelection` against the host capability into
`Option<ReasoningIntent>`, where `ReasoningIntent` is
`Effort(String) | Budget(u32) | Off`. `ProviderDefault` resolves to `None` and
sends nothing. A budget map missing an advertised effort is
`malformed_capability`, never an omission. `validate_selection`, run at the
runtime seams, is that same resolution. It never rewrites the selection.

`ReasoningCapability` is `{ efforts, encoding, disable, mandatory }`. `disable`
is a plain flag saying the route accepts an explicit off. The route's dialect
owns the wire form of off. There is no default effort in capability data: the
default is the model spec's `variant`.

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
URL selects a dialect. Lash does not guess `OpenAi` for an unknown gateway:
ADR 0072's Opper probe showed that gateways ignore wrong-shape fields, so a
guess would only move the silent drop to the gateway.

The pluggable `ReasoningWireEncoder` / `ReasoningWireFormat` machinery is
deleted. No host used a custom encoder, and a custom encoder could not
deserialize, so it could not cross the remote wire. Exotic shapes such as
`enable_thinking` belong in raw passthrough (FIG-4121), not in a trait object.

**Generation options resolve once, against the wire.**
`resolve_generation_policy` takes the request, the provider options and a
`GenerationWire` in which each adapter states what its wire carries for this
call. It refuses everything else before the adapter does any I/O:

- **Cap.** The effective cap is the request's, else
  `ProviderOptions.max_output_tokens`. With neither set, an optional-cap wire
  sends none, and Anthropic refuses with `output_token_cap_required`. Codex
  and `max_tokens_field: Omit` endpoints refuse any cap.
- **Temperature.** A wire without a temperature field refuses one. A model
  whose capability pins sampling refuses one on every adapter. Active
  Anthropic thinking, including Claude on Vertex, refuses one while an effort
  or budget is selected.
- **Seed and stop sequences** are refused on wires with no field: Anthropic
  and Responses (seed), Responses and Codex (stop), and Codex for both.
- **`parallel_tool_calls`** is a typed `Option<bool>` on `GenerationOptions`
  and replaces the hard-coded values. It is sent on Chat (with tools), on
  Responses and Codex, and on Anthropic as
  `tool_choice.disable_parallel_tool_use` (with tools). Google has no such
  control and refuses it.

Refusals carry `unsupported_generation_option`, `output_token_cap_required`,
`reasoning_encoding_unrepresentable`, `reasoning_budget_exceeds_output_cap` or
the effort-validation codes. All are `Forbidden` for retry.

Lash adds only mechanics to the wire. `store: false` and
`include: ["reasoning.encrypted_content"]` stay, because stateless replay
needs them. The `text.verbosity: "medium"` default and the hard-coded
`parallel_tool_calls` are gone.

**Validation precedes I/O.** Codex resolves every setting before its
credential manager may refresh a token. Google resolves before the credential
refresh, the project lookup and any attachment upload.

**The receipt joins provenance with emission evidence.** Resolution supplies
what the host asked for. Each adapter reports, from the branch that wrote it,
what it put on the wire, through `ResolvedGenerationPolicy::receipt`. The
receipt is not read back off the body. `Applied` means lash sent the value,
never that the provider complied. The receipt gains four rows: `reasoning`,
`parallel_tool_calls`, `thinking_summary` and `thinking_visibility`.

`OmittedSamplingPinned` is gone, because pinning now refuses the call.
`OmittedUnsupported` survives only on the `cache` row. That row reports
prompt-cache breakpoints placed by the protocol, not a host setting.

**Recorded shapes of the old path are refused.** `ReasoningCapability` and its
remote mirror deny unknown fields, so a persisted `default_effort`, `aliases`
or encoded `disable` fails to load rather than being ignored. An `OpenAiCompat`
naming `reasoning_format` is refused the same way. A recorded
`omitted_sampling_pinned` outcome no longer deserializes.

## Consequences

- A mixed-model session that sets a session-wide temperature or seed must
  clear it for models or wires that cannot carry it
  (`GenerationOverlay::Replace`). That is the cost of refusing, and the
  trade Q1 accepted: a receipt cannot un-send a call.
- Hosts on Anthropic set `ProviderOptions.max_output_tokens` or a request cap.
- Hosts that relied on a URL-selected dialect choose a preset or set
  `OpenAiCompat.reasoning`.
- Replay-route ownership is unchanged. Opaque reasoning, tool-call and
  response-item state is reusable only on the exact minting route, and dialect
  equality never makes two replay routes equal.
