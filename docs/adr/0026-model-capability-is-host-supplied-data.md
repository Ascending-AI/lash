# Model capability is host-supplied data; providers are executors

Which efforts a model accepts, its default effort, alias clamps (`minimal`→`low`,
`xhigh`→`max`), and how effort encodes on the wire (named level vs token budget) are
fast-churning reference facts about models, not provider behavior. Baking them into the
provider crates — where they had accreted as model-name sniffing behind
`ProviderModelPolicy::supported_variants` — made every model launch a lash release and left
pinned hosts with stale catalogs. We decided the runtime never derives these facts:
**capability is data the host attaches to the `ModelSpec`, and lash validates against it,
normalizes with it, and encodes from it**. `ModelCapability { reasoning }` lives in
lash-sansio (`ReasoningCapability { efforts, default_effort, aliases, encoding, disable, mandatory }`,
`ReasoningEncoding::{Effort, Budget}`) and travels spec → turn config → `LlmRequest`, with
`DirectRequest` carrying it on the direct path and the remote protocol mirroring it on
`RemoteModelIntent`/`RemoteProcessModelSpec` so remote workers behave identically.

The selected value is a closed `ReasoningSelection::{ProviderDefault, Disabled, Effort}`
rather than an optional string. Disable is separately encoded by host data as
`ReasoningDisableEncoding::{Native, Omit, Effort, Budget, ToggleFalse}`; aliases remain
effort-name normalization only.

Validation happens once, at the runtime seams (turn-driver prepare, direct client,
session-manager direct), via `ModelCapability::validate_selection`: a deterministic taxonomy
(`unsupported_effort`, `effort_not_configurable`, `effort_required`) whose snake_case codes
are a stable contract, with the alias-normalized effort written back so a provider never
sees an un-clamped value. Provider crates branch only on `encoding` and `disable` to build the wire shape
(Anthropic adaptive vs budget thinking, Gemini `thinkingLevel` vs `thinkingBudget`, OpenAI
`reasoning.effort`); the model-name checks that remain there are wire-protocol dialect facts
(request shapes, payload variants), never capability. `ProviderModelPolicy`,
`StaticModelPolicy`, and `ProviderHandle::{supported_variants, validate_variant}` are gone.

A host supplies the data from an ordered pattern-rule catalog alongside its
context-window catalog, with the same builtin-override precedence; a new model is a data
row, not code. Every host supplies its own rows (or richer sources) the same way. The trade
we accepted: hosts own the burden of knowing model facts — an unknown model simply has no
effort controls, and an explicit effort on one is rejected as `effort_not_configurable`
rather than guessed at.

Attachment acceptance follows the same rule (FIG-2357). The host supplies an
`AttachmentCapabilitySnapshot` with a revision and transport acceptance rules on
`ModelCapability`. MIME rules distinguish inline bytes, stored bytes, and external
URLs; provider-file rules carry the provider scope. Core and adapters consult that
snapshot. There are no compiled MIME tables or fallback acceptance rules; an empty
snapshot accepts no attachments.

A session retains its opening snapshot, including the rule data, in its durable
model policy. Explicit model/provider changes retain that snapshot; cold loading
uses the stored model rather than refreshing capability facts from a host catalogue.
Consequently changing the host catalogue cannot change historical attachment
rendering. New sessions can adopt the new revision. The remote protocol mirrors
all acceptance rule and source variants so workers use the same retained data.

## Amendment (FIG-4120, 2026-09-29): one resolution, exact efforts, defaults are the `variant`

[ADR 0121](0121-host-generation-settings-are-sent-or-refused.md) changes the
capability shape to `ReasoningCapability { efforts, encoding, disable,
mandatory }`:

- `default_effort` is deleted. It was never read, and the host's default is
  the model spec's `variant`.
- `aliases` and case folding are deleted. Effort names match exactly, and an
  unadvertised name is `unsupported_effort`.
- `ReasoningDisableEncoding` becomes the flag `disable: bool`. The flag says
  the route accepts an explicit off; the route's dialect owns its wire form.

Validation no longer normalizes, so nothing is written back. The four
per-provider resolvers are replaced by one,
`ModelCapability::reasoning_intent`, which returns
`Effort | Budget | Off`. Each provider maps that intent in one function per
wire, and a combination a wire cannot carry is refused.

The sentences above that list `default_effort`, `aliases` and
`ReasoningDisableEncoding`, and those that describe the alias-normalized effort
being written back, describe the state before this amendment.

## Amendment (FIG-4099, 2026-09-29): one write path for the snapshot

The attachment-acceptance snapshot is creation config: it is recorded with the
session's model when the session is created, and a reopen never replaces it —
the facade's reopen merge, which replaced the whole model (snapshot included)
when a reopen stated an explicit model, is deleted. After creation the only
writer is `update(SessionConfigPatch)`. A patch's `model` retains the session's
snapshot, as this ADR requires; the snapshot changes only through the patch's
own `attachment_acceptance` field, which the durable `ApplyConfigPatch` carries
beside the model and applies after it. Every creating path — `open()` of a new
id, `create()`, `open_with_state()`/`observe_with_state()` and the engine's own
drive-open — passes the creator's config to the catalog as
`SessionStoreCreateRequest::config`, and the store writes it as the session's
initial config head in the same transaction as the catalog row
(`SessionHeadMeta::created`). The request says which head it carries:
host-facing creation states `SessionCreationHead::Config`, so the head is on
disk before the session is first materialized, and it includes the protocol turn
options the session's protocol resolves at creation (the RLM session config
among them). A core runtime binding state it was handed states
`SessionCreationHead::CommittedByCreator`, and its first commit writes the head,
so only the row is written at admission. Admitting an id that already exists
writes no config either way. The facade forms that config in one place,
`SessionBuilder::creation_config`. A reopen reads the recorded head and writes
nothing: there is no reconciliation, no seed write and no report, and builder
config stated on a reopen is ignored. Only live policy follows an open (the
session binding, turn budget, autonomy, no-progress budget and charge safety),
and a builder provider that cannot serve the recorded pin is still refused typed
(`ProviderMismatch`) without a write. Every later change is the one durable
command, `update(SessionConfigPatch)`, which covers provider, model, prompt,
generation, attachment acceptance and plugin session config.
