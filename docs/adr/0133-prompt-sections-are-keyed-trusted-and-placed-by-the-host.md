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
it. Recorded request templates for every admitted call kind, and admission of
compaction and direct calls under their owners (§6, §8), are on main
(FIG-5259): `lash-core/src/runtime/turn_driver/prepare.rs` prepares a turn's
call and `lash-core/src/runtime/owned_call.rs` admits an owned one. Every
other prompt route is deleted (§9, FIG-5260): only the runtime builds a
call's cut and composes it, through `lash_core_execution::core_internal`
(`prompt_cut`, `compose_prompt`); the facade exports neither, and a test
composes through `lash::testing::prompt`. A resumed turn serves its recorded
before-turn decisions (§6). Request templates with live attachment slots
(§6, WIRE-SLOTS) are on main under ADR 0135: admission records literals,
attachment refs, acceptance and codecs, and the response contract. Each
attempt fills slots afresh; resolved delivery values are never admitted.

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

- the protocol's execution and output mechanics;
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

- Standard (`standard_protocol`): `execution` in the initial instructions;
- RLM (`rlm_protocol`, both channels): `execution`,
  `declarations` (over exactly the offered callable surface) in the initial
  instructions; `bound_variables`, `finalization`, `required_output`,
  `context_budget` late.

No protocol renders text for a delegated child: core has no subagent concept
([ADR 0134](0134-creating-a-session-is-explicit-only-a-fork-clones.md)). A
plugin that delegates registers its own section, which reads its own
namespace.

Protocols contribute execution mechanics only, with no persona, work style,
interaction policy selected by a tool name, or answer presentation setting.
No protocol section renders for compaction. The optional standard-compaction
plugin contributes `standard_compaction/summary_instruction` for that purpose,
late by default: its summary template, previous-summary update and explicit
focus instructions all follow this section's wrappers and host placement.
A host can replace or exclude the entire instruction. A protocol has no host
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
plan its resolved config records. Creating a session is explicit; only a fork
clones (D-SESSIONLAW, FIG-5295). A catalog fork inherits all configuration,
including the plan, from its selected retained revision. Its plugin state is
the revision's: each namespace copied, or omitted when its plugin declared
`reset` at registration (FIG-5301). Every other session,
including spawned and related children, starts with exactly the plan its
creator passes, or the neutral default when none is stated. A creator may
choose to pass its parent's plan explicitly. After creation, each session's
plan is configured independently. The plan has three parts:

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
absent, the plan overrides that name an unregistered section, and the limits.

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
recorded call loses its text.

A root is keyed by the call's owner-scoped identity (`ModelCallId`): a turn's
call is its run and ordinal, and an owned call (§8) is its owner's execution
scope and its stable key there, with no turn required. The root also holds
the call's request template (§6): its literal text in content-addressed
chunks of at most 32 KiB beside its section texts, each literal chunked on
its own, and its attachment slots in order; the call's response context
(§6): its scope, and its response contract stored by content beside the
texts; and an owned call's pinned deadline: one `AdmittedModelCall` record. A slot records its ref, position,
acceptance and codec, never a delivered value. `load_admitted_call` reads a
root back with every text and chunk verified against its address, assembles
the template byte for byte, decodes the response contract, and calls no
renderer or provider builder.

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
  `InitialInstructions` composition replaces `LlmRequest.instructions`;
  `CurrentContext` continues the projector's dedicated User prefix, separated
  by three newlines, or becomes one trailing User message without a prefix;
- the call's request template and response context, in its snapshot root.

Before admission the call is prepared once: its prompt is composed, the
protocol's before-call hook runs, and the provider of its route lowers the
request to a `RecordedRequestTemplate` (`Provider::lower`): the exact literal
JSON text it sends, with one `AttachmentSlot` wherever an attachment value
goes. A slot pins the attachment's ref, its position, the delivery forms the
route accepts for it (ADR 0135 §2) and the codec, by name and revision, that
encodes a delivery into the slot. Every slot's ref is acquired under the
call's owner before the admission commits (ADR 0135 §7).

Each attempt fills the slots and sends. The host store delivers each slot's
ref in an accepted form (ADR 0135 §3), the provider encodes each delivery
through the slot's pinned codec (`Provider::encode_slot`), and the provider
sends the literals with the encoded values in place (`Provider::send`). The
deliveries live only in that attempt (ADR 0135 §4).

`Provider::send` takes the filled body and a `ResponseContext`, and no
request (FIG-5479). The body is the only statement of what the call asks. A
request beside it would be a second one, and on a resend the two disagree:
the caller of an owned call (§8) holds its request before its sections were
composed, while the body carries them. The context holds only what reading
the response needs and the body does not carry:

- the call's scope, with the attempt this send is;
- the call's response contract, fixed when the call is lowered: the model of
  the pinned route with its metadata, the output the call asked for, and
  each offered tool's name and input schema;
- this send's stream and trace senders.

The scope and contract are fixed at admission and recorded with it. A first
send takes them as admitted and a resend reads them back, and each adds its
own senders, so a resend is read exactly as its first attempt was. A provider
that must decide from what the call asks decodes the body; an in-process
model that lowers canonically reads its request back from it
(`canonical_request`). `Provider::complete`, the one call outside
admission, lowers, builds the context from the request it lowered, and
sends.

The law the attempts keep is WIRE-SLOTS:

> Every attempt of an admitted call, on any owner, sends the template its
> admission recorded. Its literal text, route, response mode, generation
> receipt and its ordered slots (reference, position, acceptance, codec) are
> read back from the admission and are byte-for-byte the admitted ones, so a
> resend on an owner whose renderer or provider builder would produce other
> bytes sends the admitted literals. Each slot is filled, per attempt, by
> exactly one JSON value that the slot's pinned codec produced from a
> delivery of the slot's reference in a form the slot's acceptance allows
> (narrowed only by the serving provider's live file scope). Every delivery
> names the slot's content: bytes hash to the reference's id and have its
> length, and a URL or provider file serves that content unchanged. A call
> with no slot sends exactly its admitted bytes. A completed call is
> replayed with zero deliveries and zero uploads, and no delivered value is
> journaled or recorded in an admission record. Request-body evidence uses
> the template; provider text may echo delivered values (ADR 0135 §4).

Response mode is recorded once. A body's top-level `stream` boolean is
its mode; construction and reopen derive an immutable cache from that literal.
A route whose JSON carries no mode (Google's URL method or a canonical
in-process request) records `transport_stream` instead. A record carrying both
statements is refused, and no literal is rewritten to obtain its mode.

No renderer, projector or provider builder runs for a resend, so a builder
or renderer changed since sends nothing different outside the slots.
Authentication and transport are not part of the template: each attempt
binds them fresh, and a transport's framing of the body (a WebSocket's
response continuation, say) is derived from the filled bytes. A resend whose
template cannot be read back as admitted, or whose slot names a codec the
serving provider does not implement, is never sent and never rebuilt: it
settles unsent with `TurnFailureCode::AdmittedRequestUnavailable`. A request
that cannot be lowered settles unsent and admits nothing.

The call is the turn's one composition point. The execution-environment
sync builds the iteration's tool surface and composes nothing; the call's
cut offers the surface the sync installed, the protocol's facts and the
session's committed usage.

There is no separate prepare phase. A crash before admission composes again,
and the callbacks before it run again, which is safe because nothing was
sent: renderers, wrappers and checkpoint callbacks must be repeat-safe until
the call is admitted. After admission, a resend is the same call: it sends
the admitted template with fresh slot deliveries, records nothing, and calls
no renderer, wrapper, projector or hook, and a resume reinstalls the plugin state the admission
committed before the turn continues. The turn's before-turn callback
decisions commit with every phase's checkpoint, and a resumed turn is
prepared from them: no before-turn callback runs once a phase has committed. A composition that fails settles the
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
it renders for, and the owner's recorded plan places it, as it does a turn's.
A compaction call composes `Compaction` sections and a direct call exactly
the `Direct { name }` sections of its explicit purpose; neither composes a
turn's sections, and both are offered no tools (a request that names tools
is refused unsent). Their text is kept byte for byte. `llm_query`'s
instructions are its plugin's `Direct { name: "llm_query" }` section.

An owned caller may supply opaque derived section inputs, frozen for that
composition (the standard summarizer's requested focus, for example). They
are renderer inputs, never extra messages; the admission records their
composed text, request template and response contract. A resend reads that
admission and renders nothing again; slots receive fresh transient deliveries.
`Provider::send` sends the supplied live body byte for byte for that attempt.
The compaction request's derived identity still includes its source snapshot and requested instruction.

Each call is admitted under the execution that owns it: a compaction under
its session command's run, a direct call under its tool attempt or process
step. `completion.start` commits, under the owner's fence and before the
first byte is sent, the call's admission record (§5): its snapshot, its
request template and its model-total deadline. Its identity is the owner's
execution scope and the call's stable key there (`ModelCallId::Owned`). A
redrive of the owner makes the same call again: it finds the record, sends
the stored template with fresh slot deliveries under the pinned deadline, and composes, lowers and records
nothing; past that deadline it settles unsent with
`TurnFailureCode::ModelTotalExceeded`. The call's response is durable only
through the owner's own commit (the compaction's `session.command`, the
tool's `round.outcome`), as a turn's call is through `model.done`.

### 9. One route

No other route carries instruction text. A host reads its catalog and
previews its plan (`PromptCatalog::preview`); it cannot build a cut or
compose. The following are deleted:

- protocol system-prompt renderers;
- `StandardPrompt` and the protocol prompt commands;
- `ToolModule.instructions` (deleted, FIG-5258);
- the `messages` fields of turn, after-turn and after-tool contributions
  (deleted, FIG-5258);
- `TurnContextTransform` (deleted, FIG-5258);
- `RenderCompactionPrompt` (deleted, FIG-5259);
- the direct-call instruction fields (deleted, FIG-5259);
- the public cut constructors and composition entry points, the
  tool-contract markdown renderer, the turn checkpoint's synthetic-message
  counter and the context-transform turn phase (deleted, FIG-5260).

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
