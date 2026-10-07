# 0133: Prompt sections are keyed, trusted, and placed by the host

## Status

Accepted. The contract of §1 to §5 is on main (FIG-5254):
`lash-core-store/src/prompt_sections.rs` holds the recorded vocabulary, and
`lash-core-execution/src/plugin/prompt.rs` holds the registration, resolution
and chain contract. The composer, its limits and the content-shared snapshot
storage of §5 and §7 are on main too (FIG-5256):
`lash-core-execution/src/plugin/prompt/composer.rs` and the durable `prompts`
domain. The Standard and RLM protocols contribute their prompt as keyed
sections, and a turn composes them at each execution-environment sync
(FIG-5257). The rest is open work this decision depends on:

- FIG-5255: §6 admission at `model.start`;
- FIG-5258: tools, MCP, add-on plugins and the workbench;
- FIG-5259: exact provider bodies for every call kind;
- FIG-5260: deleting every other prompt route (§9).

## Context

Model-facing instruction text reaches a request by several routes. A protocol
renders a whole system prompt. Tool modules carry instruction prose. Plugin
callbacks append messages, and a prompt-view transform appends text to
history. Each route has its own owner, timing and durability, and none
records what a given call carried. A plugin that keeps state, such as a
memory, cannot say "render my current value before every call" without
appending a message. Such a message can enter committed history:
`runner::before_turn_leak_tests` in `lash-sim` pins that bug.

## Decision

### 1. One composition of keyed sections

Every piece of model-facing instruction text is a prompt section, owned as
`(plugin id, local key)` (`PromptSectionId`):

- the protocol's intro, guidance, execution and output text;
- tool and MCP guidance;
- host text;
- add-on plugin content.

A plugin registers sections through `PluginRegistrar::prompt`. Registering
one key twice in one plugin is refused. Plugins with different ids may use
the same local key. Keys are 1 to 64 bytes of lowercase letters, digits,
`_`, `-` and `.`.

Tool schemas stay typed declarations. Conversation history and tool results
stay history. Section text never enters the conversation graph.

The protocols' sections, with their default placements:

- Standard (`standard_protocol`): `intro`, `execution`, `guidance` and
  `tool_modules`, all in the initial instructions;
- RLM (`rlm_protocol`, both channels): `intro`, `guidance`, `execution`,
  `declarations` (over exactly the offered callable surface) and `subagent`
  in the initial instructions; `bound_variables`, `finalization`,
  `required_output`, `final_answer_format` and `context_budget` late.

Only `intro` and `guidance` render for a compaction. A protocol has no host
prompt config: a host adds its text as its own sections and replaces or omits
a protocol section with a wrapper.

The history a request projects is narrowed by one route only: an
attachment-omission history policy names attachment parts by message id and
part index, and core replaces each with one fixed placeholder in the
request's view. It cannot add text, change a role or touch stored history.

### 2. Trusted wrapping

Plugins are trusted. Any plugin may register a wrapper over any section, its
own or another plugin's, protocol sections included. A wrapper receives the
text the chain has produced so far and may return it unchanged, prepend,
append, replace it, or omit it. It may replace an omission.

A section's chain runs in plugin registration order, then declaration order
within a plugin. For base renderer `R` and wrappers `A` then `B`, the final
text is `B(A(R))`. A wrapper whose target is absent from a call does not run;
the call records it under `absent_targets`. The run's admission pins the
registration order. Worker completion order never affects composition.

### 3. The host owns the plan

The host's `PromptPlan` is session config, recorded with the config head and
changed only by the core command `SetPromptPlan`. A run executes under the
plan its resolved config records. The plan has three parts:

- `order`: the sections that come first, in that order. Every other section
  follows in registration order.
- `placements`: the host's `PromptPlacement` per section.
- `limits`: the bounds of §7.

`InitialInstructions` places a section in the provider's instruction field.
`CurrentContext` places it late, after the projected conversation and outside
its history: the call's late sections are one runtime-feedback (system-role)
message after everything the protocol's projector rendered. A projector
renders history only; it never places a section. A plugin declares a default placement. The host's placement
wins, and the call records whose choice each placement was
(`PlacementSource`).

Lash sets no placement policy. The trade-off belongs to the host. A section
that changes between calls changes the request prefix when placed in the
instructions. Placed late, it keeps the history prefix stable, but some
providers lower late text into tagged user content.

A plan that orders or places a section twice, or whose per-section limit
exceeds its total, is refused as `CoreConfigRefusal::PromptPlanRefused`. A
plan naming a section no installed plugin registers fails the call that
resolves it, with `PromptPlanError::UnknownSection`.

### 4. Render input is a committed cut

A renderer or wrapper reads only a `PromptInput`. It holds:

- the call's identity and purpose;
- the plugin's own namespace, frozen at one published generation
  (`CommittedPluginNamespace`);
- the plugin's admitted config;
- a read view of the committed frame;
- the tools offered to this call;
- the admitted model and the session's committed prompt usage, and history
  statistics measured before any section text is added;
- the session's recorded subagent authority;
- the protocol's facts, derived from its committed execution state (RLM's
  bound values), typed by the protocol and opaque to everything else.

It holds no writable service, no state commands and no other plugin's
namespace. A publication that lands while a renderer runs is invisible to it.
Renderers and wrappers must be repeat-safe until the call is admitted. This is
a trusted-code contract, not a sandbox.

### 5. Records

A call records a `ResolvedPromptPlan`. It holds the purpose, every selected
section in plan order with its owner revision and resolved placement, each
wrapper chain with owner revisions and ordinals, the wrappers whose target is
absent, and the limits.

It also records a version-1 `PromptSnapshot`. For each section, the snapshot
holds the base text, each applied wrapper's output in chain order, and the
final text. Text is a content-addressed `PromptTextRef`, so unchanged text is
shared across sections and calls; an explicit omission is `Omitted`. A
snapshot depends on no earlier snapshot, and nothing recomposes a request
from it. Both records change in place until the 1.0 cut. A snapshot that
states another version does not decode.

`PromptCatalog::compose` produces both records. It renders each section's
base text and then its wrapper chain, assembles each placement's final texts
in plan order joined by a blank line, and normalizes: empty text is an
omission, and any other text is kept byte for byte. A placement with no text
carries nothing.

The snapshot is stored as an audit root, one per admitted call
(`PromptWrite::Record`, tables `prompt_snapshots`, `prompt_texts` and
`prompt_snapshot_texts` in both dialects' baseline). Each text is stored once
by content address, however many calls share it. A call records once; a
second record is refused (`DomainRefusal::PromptCallRecorded`). Ending a turn,
which prunes its phase rows, leaves its roots. Only the explicit retention,
`PromptWrite::Release` for a turn or a whole session, removes roots, and a
text goes with the last root that names it. `load_prompt_snapshot` reads a
root back with every text verified against its address and calls no
renderer.

### 6. When composition runs

A prompt is composed before every new model call, including after each tool
round within a turn. Composition reads the last commit (`round.outcome` or
`turn.admit`). `model.start` commits three things in one transaction: the
pending checkpoint-callback decisions, the call's snapshot, and the exact
provider request body. There is no separate prepare phase. A crash before
admission composes again, which is safe because nothing was sent. After
admission, a resend sends the stored bytes and calls no renderer, wrapper,
projector or hook.

### 7. Failure and limits

Composition fails closed. A renderer's or wrapper's error, panic or overrun
sends no request and is typed and attributed to its site
(`PromptCompositionError`, `PromptRenderSite`). No earlier text stands in. An
error is never an omission: a renderer that means to contribute nothing
returns `SectionText::Omit`.

`PromptLimits` defaults to:

- 128 sections and 256 wrappers per call;
- 32 KiB per base text or wrapper output;
- 256 KiB of final section text;
- a 2 s render budget.

The host configures them in the plan. Oversize is refused, never truncated.

Renders run on a bounded worker pool (`PromptRenderPool`), off the caller's
scheduler. A full queue refuses the call (`RenderersBusy`) rather than
growing. When the budget passes, the call fails with `BudgetExceeded`; renders
of that call still queued never start, and one still running finishes into a
dropped result. The pool cannot stop native code that never returns: a
renderer that hangs holds its worker. Results are assembled in plan order,
never in completion order, and the first failure in plan order is the one
reported.

### 8. Compaction and direct calls

Compaction and direct calls compose through the same contract. Each uses a
purpose-specific selection (`PromptPurpose`): a section declares the purposes
it renders for. Compaction is offered no tools. Each call has its own
admission record.

### 9. One route

Once the open work lands, no other route carries instruction text. The
following are deleted:

- protocol system-prompt renderers;
- `StandardPrompt` and the protocol prompt commands;
- `ToolModule.instructions`;
- the `messages` fields of turn, after-turn and after-tool contributions;
- `TurnContextTransform`;
- `RenderCompactionPrompt`;
- the direct-call instruction fields.

Formats change in place at version 1, with no flag, shim or legacy decoder.

## Consequences

Every admitted call carries an inspectable record of exactly which text it
sent, who authored each piece, and where it went. A plugin with live state
contributes it as a section and never writes history to do so. The host
decides cache placement per section. The cost is one snapshot per call, made
small by content-addressed text, and a trusted-code obligation on renderers.

## Rejected alternatives

- **Positional transcript patches**, which append section diffs to the
  conversation. They mix instructions with chronology, need replay and
  compaction rules, and do not preserve a provider's wire prefix.
- **Last-shown fallback** on a render error. It sends stale state silently;
  a call fails closed instead.
- **Protected protocol sections.** Plugins are trusted, and replacing
  protocol text is a supported use.
- **A placement policy in lash.** Whether a volatile section belongs in the
  instructions or late is a cache and semantics trade-off only the host can
  weigh.
- **A separate prepare commit before `model.start`.** Composing from the last
  commit and admitting everything at `model.start` gives the same committed
  read without an extra transaction per call.
