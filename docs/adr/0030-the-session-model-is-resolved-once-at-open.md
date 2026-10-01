# The session model is recorded at creation

## Context

Context budgeting, prompt transforms and model calls need one authoritative
session policy. Resolving a different model in each phase makes token limits
and the request disagree. An Agent Frame records execution history and cannot
serve as mutable configuration.

## Decision

The creator supplies the session configuration, including its model and
provider pin. Catalog admission records the initial configuration head in the
same transaction as a new session row. `SessionCreationHead::Config` requests
that head; `CommittedByCreator` leaves publication of the supplied runtime
state to the creator's first commit. Rebinding an existing catalog id writes
no new configuration.

The facade separates `create(SessionCreation)` from `open()`. Create refuses
an existing id with `SessionAlreadyExists`. Open resolves an existing id,
loads its recorded configuration, and refuses an unknown or deleted id.
Opening does not select a new model or reconcile a host seed into the head.
A provider supplied at open resolves the recorded pin; an incompatible
provider is refused with `ProviderMismatch`.

`effective_policy()` reads session policy directly. `FrameOpen` assignments
are immutable history and retain the model recorded when the frame opens.
Later configuration changes are typed config commands in a durable,
revision-checked transaction (ADR 0126). The core owner's commands cover
provider, model, prompt, generation, attachment acceptance, the execution
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

The opener owns only the session binding: the store and the worker wiring
it runs on. An open, including the engine's own reopen, overrides no recorded
fact. A session-turn process's worker names the configuration that process's
session is created with; there is no implicit default.

Input admission does not select a model. Child-session execution and direct
LLM requests have explicit model selection at their own boundaries. The
runtime has no turn-level model overlay.

## Bypass surfaces

Testing-only state replacement is tooling, not a product configuration path.
A product host changes configuration through a patch. Opening a persisted,
non-current Agent Frame fails with `HistoricalAgentFrameSwitchUnsupported`;
reopening the current frame is idempotent. A protocol outcome cannot select
an old frame to overwrite current policy.

## Consequences

Reopening a session uses its durable configuration, including before its
first turn. One engine runs sessions with different turn budgets or other
execution controls, each as it recorded them. Hosts that need different
configurations create distinct sessions or submit explicit patches. A frame's
recorded model explains its history; it does not override the session's
current model.

A turn-level overlay, host-wins reopen merging, per-open overrides of the
execution controls and a plugin hook that rewrites the whole policy are
rejected because each adds a second configuration authority. Structured child
or direct requests remain explicit and do not change the parent session
policy.

## Implementation

- [Creation and open](../../crates/lash/src/session.rs), including recorded-state loading and provider-pin validation.
- [Creation head contract](../../crates/lash-core-store/src/session_identity.rs).
- [Session policy and immutable frames](../../crates/lash-core-store/src/session_state.rs).
- [Durable configuration application and the per-root snapshot](../../crates/lash-core/src/runtime/drive/turn_config.rs).
- [Recorded configuration and its patch](../../crates/lash-core-store/src/session_policy.rs).
