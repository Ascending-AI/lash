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
Each refusal names the route and tier, directs authors to process-step
decomposition, and notes the pending intent protocol.

The 20-row attempt-atomicity matrix and its controller-decorating sentinel are
the standing inventory mechanism. The sentinel records every controller
crossing while a recorded attempt body is open; its catch-all law fails when a
new capability crosses without a declared row. Restate endpoint laws separately
pin the ordinal behavior, including active crash-redrive laws for the formerly
unguarded await, cancel, and signal routes. The PostgreSQL crash/replay law
proves the converse: its controller is key-addressed by stable replay key, so
nested commands remain safe and all guarded capabilities stay available there.
Runtime-owned tiers are likewise unaffected.

This guard is the fail-closed backstop, not the final authoring model. The
capability-separated result/intent protocol described by the determinism survey
remains the intended redesign; track it as **FIG-INTENT (to be filed)**. The
separate journal-addressing and durable-workflow capabilities should eventually
move onto one consolidated controller-traits surface without conflating their
semantics.

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
