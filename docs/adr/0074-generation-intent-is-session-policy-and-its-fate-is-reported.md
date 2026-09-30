# Generation intent is session policy, and its fate on the wire is reported

## Context

Sampling and output controls must reach every call made for a session, including
child and direct calls. A host also needs to distinguish the options it requests
from what Lash sends and from what a provider reports about execution.

## Decision

Generation intent belongs to `SessionPolicy.generation`. Creation resolves the
`SessionCreation.spec` overlay against the core policy and records it with the
session's initial configuration. Only `create` creates a facade session. Open
loads the recorded configuration; it does not reconcile a new generation
setting, model, or session prompt into it. Later changes use
`update(SessionConfigPatch)`.

`GenerationOverlay::Merge` keeps unstated options. `Replace` discards them, and
replacing with default options clears the intent. The same vocabulary applies
to creation, child-policy resolution, and configuration patches. A child that
sets only its cap therefore keeps an inherited temperature and seed unless it
explicitly replaces them.

The durable config includes the generation controls and session prompt. The
live core prompt remains a separate base layer rendered on each request. A
legacy absent session prompt reconstructs as an empty session layer; explicit
emptiness also remains empty. Provider handles on open resolve the recorded
provider pin and cannot silently replace it.

Every request taken from session policy pairs its generation options with the
request's model. `ModelSpec` clamps a requested output cap to that model's
capacity without rewriting stored intent. A cap is an upper bound, so using a
smaller capacity satisfies bounded execution while reducing the requested
allowance. The runtime records `ClampedToCapacity` on the response and attempt
receipts. Direct calls through a tool's `AttemptContext::direct_completions`
carry explicit request intent; a tool selecting a different model owns that
pairing.

Provider resolution layers its configured cap beneath request intent exactly
once. It invents no cap or temperature. Unsupported explicit controls are typed,
non-retryable refusals before I/O under
[ADR 0121](0121-host-generation-settings-are-sent-or-refused.md). A mixed-model
session uses an explicit replacement or update to clear incompatible intent.

## Receipts and protocol-owned boundaries

`GenerationReceipt` joins resolved intent with adapter emission evidence.
`Applied` means sent, not provider compliance. Receipts accompany responses,
attempt accounting, durable effects, tracing, and remote responses.
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
