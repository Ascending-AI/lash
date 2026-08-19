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
