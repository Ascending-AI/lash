# The session model is recorded at creation

## Context

Context budgeting, prompt transforms and model calls need one authoritative
session policy. Resolving a different model in each phase makes token limits
and the request disagree. An Agent Frame records execution history and cannot
serve as mutable configuration.

## Decision

The creator supplies the session configuration, including its model: a key
the core's registry mints into the recorded binding. Catalog admission
records the initial configuration head in the
same transaction as a new session row. `SessionCreationHead::Config` requests
that head, and every session creator states it: the host's `create` and the
session manager's create, whose first commit publishes over it. A catalog row
with no head recorded no configuration; opening it is refused as
`SessionCreationUnrecorded`, never served from defaults. Rebinding an existing
catalog id writes no new configuration.

The facade separates `create(SessionCreation)` from `open()`. Create refuses
an existing id with `SessionAlreadyExists`. Open resolves an existing id,
loads its recorded configuration, and refuses an unknown or deleted id.
Opening does not select a new model or reconcile a host seed into the head.
An open supplies no model: the recorded binding stands, and its key binds
against the core's registry when a request runs. A key the registry does not
serve, or one now serving another wire model, is refused `ModelUnavailable`.

`effective_policy()` reads session policy directly. `FrameOpen` assignments
are immutable history and retain the model recorded when the frame opens.
Later configuration changes are typed config commands applied through a
durable, revision-checked `ConfigTransaction` (ADR 0126). The core owner's commands cover
model and reasoning, prompt, generation, attachment acceptance, the execution
controls and tool access; each plugin's commands cover its own namespace.

The execution controls are the turn budget, autonomy, the no-progress budget
and charge safety. They are session configuration like the model. The creator
states them in `SessionCreation::spec`; unset controls take the core's
creation defaults. Each root snapshots the configuration, controls included,
in its recorded `ResolveTurnConfig` step (`ResolvedRun`). Its turns, redrives,
replays and a recovered follow-on run under that snapshot, so a later
configuration change reaches the next root, never a running or replayed one.
An urgent stop is a recorded cancellation, not a configuration change.

Remote process environments carry the recorded no-progress budget and charge
safety alongside the turn budget and autonomy. A peer requires both fields;
it never substitutes its defaults. Session creation and `SetChargeSafety`
share the ceiling check and refuse `UnsafeRetriesAboveCeiling` before
publishing a configuration. The provider handle applies the admitted retry
limit without a separate clamp (FIG-4480).

A root's `ResolvedRun` also carries the host's termination policy
(`TerminationPolicy`) as it stood at the root's first execution. Terminal
assembly reads the record,
so a worker with another policy assembles the same terminal for a turn whose
stream ended without `Done`. The policy stays host configuration: a change
reaches roots that start after it.

The system prompt is recorded the same way as the rest of the configuration.
It is the protocol plugin's: core has no prompt type. A session's protocol
namespace records its prompt config when the session is created, from the
plugin creation options of the session's `SessionSpec` laid over the creating
core's default spec, and the protocol's prompt commands change it for the
roots after them (ADR 0126). The protocol renders the system prompt from the
namespace the running root was admitted under, and the render is a recorded
step: a redrive is served the recorded text and renders nothing. A child
created by its parent copies the parent's recorded prompt config, and a
process carries the recorded plugin config of its starter. A run's options
cannot state a prompt: a run-options payload that carries one is refused,
typed (FIG-4589).

A compaction's summarizer call carries the prompt the same protocol renders
for it, without tool or execution prose since the call ships no tools. That
text is recorded before the call on every compaction path (commanded,
context pressure and overflow recovery), so a redrive replays it.

The opener owns only the session binding: the store and the worker wiring
it runs on. An open, including the engine's own reopen, overrides no recorded
fact. A session-turn process's child is created from its starter's recorded
facts: the start captures its starter's recorded configuration, admits the
child's complete facts against it before the handoff, and the worker creates
the child from that captured environment. A worker names no configuration of
its own (ADR 0088).

The recorded model is bound to its transport lazily. Only the body of an
unjournaled model call or direct completion asks the host's models for the
transport; a replay serves the recorded call and asks nothing, so a
deployment that retired the key still completes recorded work (ADR 0105 §1).
A bind the deployment refuses is the attempt's fault. It is never the call's
recorded result: the step stays unsealed, the engine retries it, and after
its attempts it parks the work with reason `EngineRetryExhausted` carrying the
model key typed. A direct completion inside a tool attempt ends that attempt
the same way, whatever the tool makes of the error. A deployment that serves
the key again lets a resume proceed (FIG-4404).

Input admission does not select a model. Child-session execution and direct
LLM requests have explicit model selection at their own boundaries. An input
may carry a `RunSpec` whose recorded overrides — route, model, generation,
prompt layer, protocol turn options — run that input's root under them without
changing the session's recorded configuration (ADR 0101 §A5): the override is
durable input data the root's admission fixes, not a mutable overlay on
session policy.

## Bypass surfaces

Testing-only state replacement is tooling, not a product configuration path.
A product host changes configuration only through a typed `ConfigTransaction`
submitted under `ConfigWrite { id, expected_revision }` (ADR 0126). Opening a persisted,
non-current Agent Frame fails with `HistoricalAgentFrameSwitchUnsupported`;
reopening the current frame is idempotent. A protocol outcome cannot select
an old frame to overwrite current policy.

## Consequences

Reopening a session uses its durable configuration, including before its
first turn. One engine runs sessions with different turn budgets or other
execution controls, each as it recorded them. Hosts that need different
configurations create distinct sessions or submit typed `ConfigTransaction`s.
One engine likewise runs sessions created with different prompts, and a
redeployed core's prompt reaches only the sessions it creates. A frame's
recorded model explains its history; it does not override the session's
current model.

An overlay that lets a turn rewrite the recorded policy, host-wins reopen
merging, per-open overrides of the execution controls and a plugin hook that
rewrites the whole policy are rejected because each adds a second
configuration authority. A `RunSpec`'s per-root overrides are not one: they
are recorded input data fixed at admission that shape only their own root
(ADR 0101 §A5). Structured child
or direct requests remain explicit and do not change the parent session
policy.

## Implementation

- [Creation and open](../../crates/lash/src/session.rs), including recorded-state loading and recorded-model binding.
- [Creation head contract](../../crates/lash-core-store/src/session_identity.rs).
- [Session policy and immutable frames](../../crates/lash-core-store/src/session_state.rs).
- [Durable configuration application and the per-root snapshot](../../crates/lash-core/src/runtime/drive/turn_config.rs).
- [Recorded configuration and its typed commands](../../crates/lash-core-store/src/session_policy.rs).
