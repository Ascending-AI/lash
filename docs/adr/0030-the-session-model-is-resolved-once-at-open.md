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
