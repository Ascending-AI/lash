# Tool attempts are atomic

Tool implementations are opaque host code. Lash cannot reliably discover,
name, order, or replay every network call, database write, timer, or other side
effect performed while a tool runs. Pretending that those operations compose
as nested durable effects creates a leaky boundary whose behavior differs by
execution tier.

The contract is therefore:

> Attempts are atomic. In-attempt effects are opaque. Durable composition lives
> in the process layer.

One prepared tool attempt is one journaled `ToolAttempt` entry. Everything the
tool performs before returning its result belongs to that entry, including a
direct completion and a retry delay. A direct completion still uses the normal
session-manager request plan and the normal post-outcome usage, trace, and
bookkeeping path, but it executes locally while the parent attempt is open. It
does not submit a nested `Direct` envelope. This decision depends only on the
operation's position inside a tool attempt, so inline and workflow-backed tiers
follow the same path.

The two-line implementation law is:

> `DirectLocal` execution is reserved for Lash-owned deterministic interpreters
> (`ExecCode` and `ToolBatch`).
> Opaque host code gets one atomic journaled entry per attempt.

`ExecCode` and `ToolBatch` may be rebuilt during workflow replay because Lash
owns their interpreters and their nested atomic effects have stable identities.
That exception does not extend to a `ToolProvider::execute` implementation.
Nested tool-batch dispatch from a tool attempt remains prohibited; authors must
decompose that composition into process steps.

FIG-1127 completion (2026-08-12) replaces the falsified static route inventory
with this structural rule:

> A recorded body must not emit commands into an ordinal-addressed journal.

Every process command emitted through an in-turn process capability crosses
`ProcessCommandRunner::run`. That choke point refuses `Start`, `Await`,
`Cancel`, `Signal`, and `Transfer` when their parent is a `ToolAttempt` and the
controller reports `EffectJournalAddressing::OrdinalAddressed`. `List` remains
available: the Restate endpoint replay law proves at the captured-byte level
that it is registry-served and reissues no journal command. Registry-only
`validate_visible` and `complete_external` do not cross the choke point.
`ToolContext::triggers().emit()` and `ToolContext::sessions().start_turn()` keep
their corresponding ordinal-tier guards because they are not process commands.
Each refusal names the route and tier and directs legacy authors to process-step
decomposition.

The 20-row attempt-atomicity matrix and its controller-decorating sentinel are
the standing inventory mechanism. The sentinel records every controller
crossing while a recorded attempt body is open; its catch-all law fails when a
new capability crosses without a declared row. Restate endpoint laws separately
pin the ordinal behavior, including active crash-redrive laws for the formerly
unguarded await, cancel, and signal routes. The PostgreSQL crash/replay law
proves the converse: its controller is key-addressed by stable replay key, so
nested commands remain safe and all guarded capabilities stay available there.
Runtime-owned tiers are likewise unaffected.

FIG-1291 ships the capability-separated authoring model. A provider opts in with
`supports_attempt_context` and receives the sealed, controller-free
`AttemptContext`. A completed attempt returns `ToolAttemptResult::Done` with a
`ToolResultDone` and versioned `ToolIntents`; a deferred attempt returns
`ToolAttemptResult::Pending`, whose type cannot carry intents. Lash records the
final attempt first, then admits and drains its declarations in source order.
Retries discard non-final declarations. Each v1 declaration derives one stable
identity from `(session_id, execution_scope_id, tool_call_id, intent_index)`;
the execution-scope component is the turn id in turn scope and the process id
in process scope. FIG-1203 remains the rebase point for frame-key-grade call
identity.

Realization is journal-first. The recorded intent issues exactly one process
command with its identity-derived replay key. It does not re-read visibility,
existence, terminal state, the live tool filter, or live host configuration
before that durable boundary. Deterministic admission uses only recorded data.
Unknown/terminal/conflict results are recorded command outcomes, so redrive
replays identical evidence even if the live registry changes. A start's env
spec, observers, input, and parent-end policy all come from the recorded
payload. Tool visibility is therefore an authoring-time catalog decision, not
a replay-time intent gate; a future contract requiring such a gate must record
it as a separate admission fact.

After the enclosing turn or process reaches its end, recorded start intents are
handled by a deterministic parent-end step: `Abandon` emits no command, while
`Cancel` and `Terminate` emit one replay-keyed cancellation command with a
policy-specific reason. The old `ToolContext` guards remain only for providers
that have not yet moved to the FIG-1291 leaf signature.

The former `ToolContext::durable_effects()` facade and its `DurableStep`
producer are removed, including the serialized command and outcome. External
waits use deferred tool completion when the whole job has one eventual tool
result. Workflows with multiple durable effects, waits, or decisions model each
boundary as a process step and pass durable data between those steps.

Atomic attempts provide replay after a completed outcome: replay returns the
recorded tool result without invoking the provider or any other in-attempt work
again. They cannot provide exactly-once execution across an unrecorded
completion. If an attempt is retried, or the worker crashes after an
in-attempt effect succeeds but before the attempt outcome is recorded, the
whole attempt runs again. In-attempt effects are consequently at-least-once;
an LLM call can be billed again. Tool authors must make external writes
idempotent when needed and move independently durable boundaries into process
steps.
