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

The controller-owned replay boundary is enforced at every `ToolContext` route
that can otherwise enter another journaled command. The inventory is:

| Route from a tool body | Resolution |
| --- | --- |
| `ToolContext::dispatch().batch()` | Refuses nested dispatch and directs the author to process steps. |
| `ToolContext::processes().start()` | Refuses before process registration or workflow start; start the process from a process step. |
| `ToolContext::triggers().emit()` | Refuses before occurrence ingestion or delivery routing; emit the trigger from a process step. |
| `ToolContext::sessions().start_turn()` | Refuses before turn-scope registration; start the nested turn from a process step. |
| `ToolContext::direct_completions()` | Executes locally at `DirectExecutionPosition::ToolAttempt`, without a nested journal command. |

These refusals apply specifically when a controller owns replay for the parent
`ToolAttempt`. Inline execution has no controller journal to nest and retains
its existing behavior.

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
