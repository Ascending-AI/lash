# Model capability is host-supplied data and providers are executors

## Decision

Model capabilities are host-supplied data attached to `ModelMetadata`, recorded with the session's `RecordedModel` when the host's model registry mints it for a `ModelKey`, passed into turn configuration, direct requests and `LlmRequest`, and mirrored by the remote protocol. Providers execute the declared contract and encode it for their route. Capability facts do not come from provider model-name guesses.

`ReasoningCapability` contains exact accepted `efforts`, `encoding`, `disable` and `mandatory`. Selection is `ReasoningSelection::{ProviderDefault, Disabled, Effort}`. A host default is the reasoning selection recorded with the session's `ModelConfig`. Effort names match exactly; there is no alias normalization or case folding. `disable` states whether explicit off is accepted; its wire form belongs to the route.

## Rules and guarantees

`ModelCapability::reasoning_intent` supplies one shared classifier, returning no explicit control or `Effort`, `Budget` or `Off`. Validation and wire mapping use that result. Failures retain typed categories: `unsupported_effort`, `effort_not_configurable`, `effort_required` and `malformed_capability`. An encoding that omits an advertised budget is malformed. Each provider maps the intent into its wire and refuses combinations it cannot represent under ADR 0121.

Attachment acceptance follows the same rule. `AttachmentCapabilitySnapshot` carries revisioned transport rules for inline bytes, stored bytes, URLs and scoped provider files. An empty snapshot accepts no attachments. The session records the snapshot as its own core config field, beside the model rather than inside it. Reopen reads it without replacement. A `SetModel` command retains it; only `SetAttachmentAcceptance` changes it explicitly. Remote workers receive the retained rule data.

## Why and alternatives

Compiled model catalogs in provider code are rejected because model facts change independently of provider execution and pinned hosts need their own catalog updates. Guessing an unknown model's effort is rejected; explicit effort without capability is `effort_not_configurable`. Refreshing snapshots on reopen is rejected because it changes historical attachment rendering.

## Consequences

Hosts own model reference data and its update policy. Providers own route dialect and serialization. Creation records durable capability facts; later changes use the typed core config commands (`SetModel`, `SetReasoning`, `SetAttachmentAcceptance`; ADR 0126). [Capability types and classification](../../crates/lash-sansio/src/llm/capability.rs), [model snapshot preservation](../../crates/lash-core-store/src/session_policy.rs) and [session creation](../../crates/lash/src/session.rs) implement the rule.
