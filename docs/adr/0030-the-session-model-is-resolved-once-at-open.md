# The session model is resolved once, at session construction

A session's model had three competing sources: the core builder's required model, a
turn-level overlay (`TurnBuilder::model`), and two persisted copies with contradictory
authority — the facade overwrote top-level `state.policy` with the builder's model on
reopen, while the current Agent Frame's persisted assignment then defeated both through
`effective_policy()`. The overlay was also applied late: pre-turn context budgeting and
transforms read the session's effective model before the overlay existed, so any host that
relied on the overlay fed its context machinery the wrong token limits. We decided there is
**one resolution point: the host supplies the Session Model when it constructs or reopens a
session, and that value is reconciled into every live and stored copy (top-level policy and
current frame assignment) before the runtime starts.** Historical frames keep the models
they ran with; they are durable history, not configuration.

There is no turn-level model overlay. `TurnBuilder::model` and the `TurnContext` override
are deleted. Per-execution variation is expressed by resolving a different model at
construction — hosts that submit each turn as its own execution get per-turn selection with
nothing extra; a durable mid-session change goes through the config-update door, which
updates the runtime, state policy, and current frame together and persists. Heterogeneous
work inside a run keeps its existing explicit seams: subagent/child tiers and process
execution specs carry their own models, and `DirectRequest` carries its own. What is
forbidden is ambient mutation — a model that changes depending on which phase or consumer
reads it — not structured routing.

Runtime admissions stay model-free: Pending Turn Input and Queued Work carry no model, so
admission evidence never becomes a second durable model copy. When intent is bound to
queued input is the host queue's decision; a host that binds at enqueue owns persisting
that binding and supplying it at construction time.

The persisted Session Model means "what this session last executed with." A host may read
it back as its own default — that is a host choice, and the only way stored state
influences selection. Consequently the agent-turn remote envelope no longer carries a model
intent (it was validated and discarded, and could never build a complete spec); the
direct-request envelope keeps its consumed intent, and removing the field is a remote
protocol version bump, not a silently tolerated unknown field.

## Immutable-frame amendment

ADR 0047 removes the second mutable copy on which this ADR's reconciliation
mechanics depended. A `FrameOpen` model assignment is immutable history: reopen
and config update do not rewrite it, and `effective_policy()` reads only the
session policy.

The one-resolution-point ruling survives and is stronger. Session policy is the
single live configuration copy. The config-update door updates the runtime and
`state.policy`; the next `FrameOpen` captures that current policy, while
existing frames retain the model they opened with. This supersedes the
requirements above to reconcile or update a current frame assignment together
with policy. It does not restore a turn-level overlay.

## Seed-then-write amendment (FIG-1896)

The open-time host precedence this ADR establishes is an *initialization*
rule, not a standing runtime authority over the durable head. On a reopen the
host-supplied values act as a seed: they are reconciled into the resident
policy once, and any difference from the persisted head is then guard-written
to the durable head before the session is observable as open
(`LashRuntime::settle_reopen_seeded_config`, FIG-1875's settlement path). From
that point on the head is true again and every later adoption through
`adopt_durable_head` is unconditional head-wins — there is no facade-reopen
carve-out and no resident copy that runs ahead of the durable record.

## Bypass surfaces

FIG-1875 made session configuration a durable fact that changes only through a
commanded config patch settled at the command-queue drain; resident policy never
runs ahead of the durable head. Two pre-existing paths replaced resident
configuration with no guard write, and FIG-2520 disposes of both:

- `SessionStateAdmin::set_persisted` (facade) and
  `LashRuntime::apply_persistence_state` (core) replace resident state without
  durable publication. Both exist only under the `testing` feature. They are
  test and recovery tooling, never a product path, and are never blessed with a
  guard write: a product host that needs a different configuration issues a
  config patch.
- Opening an Agent Frame whose key names a persisted, non-current frame
  previously made that frame current and copied its recorded assignment policy
  and protocol turn options over resident state. The runtime now refuses with
  `RuntimeErrorCode::HistoricalAgentFrameSwitchUnsupported`, and a protocol
  outcome naming such a frame aborts the turn commit before any durable write.
  Historical frames stay durable history; switching one back into service would
  require a commanded config patch that nothing supports today. Reopening the
  current frame remains an idempotent no-op.

## Amendment (FIG-4099, 2026-09-29): config is baked at creation

The resolution point moves from "construction or reopen" to creation alone. The
host supplies the session's config when it creates the session, and the recorded
config is authoritative on every reopen. Every creating path — `open()` of a new
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

This supersedes the sentence above that the host supplies the Session Model
"when it constructs or reopens a session", and the whole seed-then-write
amendment (FIG-1896): `settle_reopen_seeded_config`, its content-addressed
reopen seed operation and the facade's host-wins/persisted-wins merge
(`reconcile_loaded_state_policy`) are deleted. The persisted Session Model is
no longer only "what this session last executed with" for a host to read back:
it is the session's model until a config patch changes it.
