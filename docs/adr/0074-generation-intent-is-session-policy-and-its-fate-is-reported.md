# Generation intent is session policy, and its fate on the wire is reported

## Context

Sampling and output controls must reach every call made for a session, including
child and direct calls. A host also needs to distinguish the options it requests
from what Lash sends and from what a provider reports about execution.

## Decision

Generation intent belongs to `SessionPolicy.generation`. Creation records what
`SessionCreation.spec` states with the session's initial configuration; a core
keeps no default generation (ADR 0030, FIG-4594), and a fork records its fork
point's. Only `create` creates a facade session. Open
loads the recorded configuration; it does not reconcile a new generation
setting, model, or session prompt into it. Later changes use the
core `SetGeneration` config command (ADR 0126).

`GenerationOverlay::Merge` keeps unstated options. `Replace` discards them, and
replacing with default options clears the intent. The same vocabulary applies
to creation against an explicitly supplied base and to `SetGeneration`.
A host applying a cap-only merge to a base copied from a parent keeps that
base's temperature and seed. `SessionCreation::child_of` itself copies no
configuration; a fork copies the configuration recorded at its fork point.

The durable config includes the generation controls, the core `PromptPlan`
and every plugin's namespace. The creator states the prompt plan separately
from plugin creation options; protocols contribute keyed sections. Each new
model call composes under the recorded plan, and its admission fixes the
composition for redrive of that call (ADRs 0030 and 0133). On open, the recorded
model binds back to its transport by its recorded key and cannot be silently replaced.

Every request taken from session policy pairs its generation options with the
request's model. `LlmProfileMetadata` clamps a requested output cap to that model's
capacity without rewriting stored intent. A cap is an upper bound, so using a
smaller capacity satisfies bounded execution while reducing the requested
allowance. The runtime records `ClampedToCapacity` on the response and attempt
receipts. Direct calls through a tool's `AttemptContext::direct_completions`
carry explicit request intent; a tool selecting a different model owns that
pairing.

Provider resolution layers the recorded model's default cap
(`LlmProfileRequestDefaults::max_output_tokens`) beneath request intent exactly
once. It invents no cap or temperature. The model's other behavioural defaults,
thinking visibility, prompt-cache retention and the response-metadata
allowlists, are recorded with the model in the same `LlmProfileRequestDefaults` and
ride every `LlmRequest`, so a re-sent call sends and captures what was recorded
rather than whatever the provider handle is configured with now (FIG-4374,
FIG-4397). `ProviderOptions` keeps only transport concerns: reliability and
response budgets. Unsupported explicit controls are typed,
non-retryable refusals before I/O under
[ADR 0121](0121-host-generation-settings-are-sent-or-refused.md). A mixed-model
session uses an explicit replacement or update to clear incompatible intent.

## Receipts and protocol-owned boundaries

`GenerationReceipt` joins resolved intent with adapter emission evidence.
`Applied` means sent, not provider compliance. Receipts accompany responses,
attempt accounting, durable effects, and tracing.
`ExecutionEvidence` remains provider-reported execution facts under ADR 0031.
An absent receipt means unreported, not that nothing is requested.

A protocol projector can suppress caller stop sequences when a provider stop
would truncate protocol grammar. The projected request carries local provenance
for this suppression. The runtime narrows the response and attempt receipts to
`SuppressedProtocolOwned`; an empty requested list remains `NotRequested`.

The cache row reports protocol-placed cache breakpoints. A wire that cannot emit
them reports `OmittedUnsupported`. This cache observation does not waive the
pre-I/O refusal rule for explicit generation settings.

`nothing_omitted()` detects omissions and protocol suppression.
`fully_honored()` additionally detects capacity clamping. Neither asserts that
the provider obeys a field it receives.

## Alternatives considered

Wholesale replacement by default would discard inherited options whenever a
child or patch states one field. Merge-by-default preserves that intent.
Per-option clearing is not part of the overlay vocabulary; explicit replacement
makes discarding inherited intent visible. Silent omission followed by a receipt
cannot undo a sent call, so unsupported controls are refused before dispatch.

## Consequences

Creation and durable updates own session configuration. Reopen preserves it.
Calls carry model-paired intent, and hosts can inspect receipt outcomes without
confusing request emission with provider execution evidence.

## Code references

- `crates/lash/src/session.rs:49-74,152-169,247-275` separates creation from open.
- `crates/lash-core-llm/src/provider/options.rs:299-445` resolves controls and joins receipts.
- `crates/lash-core-store/src/session_identity.rs` owns persisted config projection.
- `crates/lash-core-store/src/session_policy.rs:357-390` defines generation overlays.
- `crates/lash-core/src/runtime/turn_driver/streaming/support.rs` applies cap and stop-suppression provenance.
