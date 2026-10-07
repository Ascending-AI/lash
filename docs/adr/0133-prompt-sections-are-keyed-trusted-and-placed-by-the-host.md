# 0133: Prompt sections are keyed, trusted, and placed by the host

## Status

Accepted. The contract of §1 to §5 is on main (FIG-5254):
`lash-core-store/src/prompt_sections.rs` holds the recorded vocabulary, and
`lash-core-execution/src/plugin/prompt.rs` holds the registration, resolution
and chain contract. The composer, its limits and the content-shared snapshot
storage of §5 and §7 are on main too (FIG-5256):
`lash-core-execution/src/plugin/prompt/composer.rs` and the durable `prompts`
domain. The Standard and RLM protocols contribute their prompt as keyed
sections (FIG-5257). Tool and MCP guidance, host exclusion and readback, and
the workbench's sections are on main (FIG-5258), and §9's plugin message and
context-overlay routes and `TurnContextTransform` are deleted. §6, admission
at `model.start`, is on main (FIG-5255): `lash-core/src/runtime/durable/phases.rs`
admits each call and `lash-core/src/runtime/turn_driver/prompt.rs` composes
it. The rest is open work this decision depends on:

- FIG-5259: exact provider bodies for every call kind;
- FIG-5260: deleting every other prompt route (§9).

## Context

Model-facing instruction text reaches a request by several routes. A protocol
renders a whole system prompt. Tool modules carry instruction prose. Plugin
callbacks append messages, and a prompt-view transform appends text to
history. Each route has its own owner, timing and durability, and none
records what a given call carried. A plugin that keeps state, such as a
memory, cannot say "render my current value before every call" without
appending a message. Such a message could enter committed history; the
`prompt_section_leak` integration law in `lash` pins that section text never
does, compaction included.

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

A plugin whose sections follow the offered surface registers a family
instead (`PromptRegistrations::family`): a key prefix and a
`PromptSectionSource` that derives each call's sections from the call's
`OfferedTools`, keyed `prefix.suffix`. A section exists only for a call that
offers its surface. A tool plugin's guidance is a family over the offered
manifests. MCP guidance is the `mcp/server.<server>` family: each imported
manifest pins its server's guidance by digest at admission, and rendering
reads the stored text and never contacts the server. A family's prefix may
not overlap another family's prefix or a section's key in the same plugin.

Tool schemas stay typed declarations. Conversation history and tool results
stay history. Section text never enters the conversation graph.

The protocols' sections, with their default placements:

- Standard (`standard_protocol`): `intro`, `execution` and `guidance`, all
  in the initial instructions;
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
plan its resolved config records. Every child session starts with a copy of
its parent's committed plan (D-PSECREV, FIG-5273); a catalog fork copies the
plan of its selected retained revision. After creation, each session's plan
is configured independently. The plan has three parts:

- `order`: the sections that come first, in that order. Every other section
  follows in registration order.
- `placements`: the host's `PromptPlacement` per section.
- `limits`: the bounds of §7.

`InitialInstructions` places a section in the provider's instruction field.
`CurrentContext` places it late, after the projected conversation and outside
its history: the call's late sections are one User-role message (FIG-5271,
D-PSECREV). RLM's projector contributes only its uncached iteration header
and turn causes as that message's history prefix; placement continues it with
sections in plan order. Standard has no such prefix and gets one trailing User
message. A projector renders history only; it never places a section.
`Excluded` drops a section:
the call records the section and its placement like any other, but neither
its renderer nor a wrapper over it runs, and those wrappers are recorded
under `absent_targets`. A plugin declares a default placement. The host's
placement wins, and the call records whose choice each placement was
(`PlacementSource`).

Lash sets no placement policy. The trade-off belongs to the host. A section
that changes between calls changes the request prefix when placed in the
instructions. Placed late, it keeps the history prefix stable, but some
providers lower late text into tagged user content.

A plan that orders or places a section twice, or whose per-section limit
exceeds its total, is refused as `CoreConfigRefusal::PromptPlanRefused`. A
plan naming a section no installed plugin registers is refused when
`SetPromptPlan` resolves, with the typed `PromptPlanError::UnknownSection`
naming the id; a key under a family's prefix counts as registered. If a
plugin has since been removed, a call skips its recorded plan's overrides
and records their section ids under `absent_overrides`. Such an override
never fails a turn.

A host reads its plan and catalog back through `session.admin().prompt()`:
`plan()` returns the recorded plan, `catalog()` the installed sections,
families and wrappers, and `preview(purpose, offered)` the resolution a call
offered those tools would record, without rendering. `snapshot(run, call)`
reads that session's retained snapshot and verified text for a model call.

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
absent, the plan overrides whose sections are no longer registered, and the limits.

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
text goes with the last root that names it. Deleting a session is that
retention: the delete releases every root of the session in its own
transaction (FIG-5272). On PostgreSQL a record and a release that share a
text serialize on that text's advisory lock, shared for the record and
exclusive for the release, so neither fails the text's foreign key and no
recorded call loses its text. `load_prompt_snapshot` reads a
root back with every text verified against its address and calls no
renderer.

### 6. When composition runs

A prompt is composed before every new model call, including after each tool
round within a turn. Composition reads the last commit (`round.outcome` or
`turn.admit`) and what the turn published since it: the decisions of the
checkpoint callbacks that ran after it. `model.start` commits, in one
transaction:

- the call's identity: its ordinal among the turn's model calls
  (`ModelPin::call`, the turn row's `model_calls`);
- the turn's plugin state, the pending checkpoint-callback decisions among
  it, in the checkpoint that re-delivers the call;
- the call's snapshot (`PromptWrite::Record`);
- the request, with the composed text lowered into it: the
  `InitialInstructions` text after the request's own instructions, the
  `CurrentContext` text as one system message after the conversation. FIG-5259
  extends this to the exact provider body.

The call is the turn's one composition point. The execution-environment
sync builds the iteration's tool surface and composes nothing; the call's
cut offers the surface the sync installed, the protocol's facts and the
session's committed usage.

There is no separate prepare phase. A crash before admission composes again,
and the callbacks before it run again, which is safe because nothing was
sent: renderers, wrappers and checkpoint callbacks must be repeat-safe until
the call is admitted. After admission, a resend is the same call: it sends
the admitted request, records nothing, and calls no renderer, wrapper,
projector or hook, and a resume reinstalls the plugin state the admission
committed before the turn continues. A composition that fails settles the
call unsent with `TurnFailureCode::PromptCompositionFailed`, and protocol
facts that cannot be derived settle it with their own code. A live store
fault aborts the activation instead; it admitted nothing, so the resume
composes again.

A tool member's state resolutions commit with its `round.outcome`
(FIG-5266), and a resume publishes them from that record before the round is
presented, so the next call composes over them even when a crash falls
between that outcome and the call's admission.

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
scheduler, shared by every session of the process. A full queue refuses the
composition (`RenderersBusy`) rather than growing; that is a live fault
(`RuntimeErrorCode::PromptRenderersBusy`), never the call's outcome: the
activation ends unadmitted and its resume composes again. The budget bounds
the call's whole composition from its submission, so time queued behind other
sessions' renders counts against it. When the budget passes, the call fails
with `BudgetExceeded`; renders
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
- `ToolModule.instructions` (deleted, FIG-5258);
- the `messages` fields of turn, after-turn and after-tool contributions
  (deleted, FIG-5258);
- `TurnContextTransform` (deleted, FIG-5258);
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
