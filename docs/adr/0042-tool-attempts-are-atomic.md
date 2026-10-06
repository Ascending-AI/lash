# Tool attempts are atomic

## Context

A tool body is opaque host code. Lash cannot discover or independently record
each network call, database write or timer inside it. A body is one admitted
execution with a started row and an outcome
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §5); nested
durable commands inside it would have no phase of their own to commit.

## Decision

One prepared tool attempt is one recorded `ToolAttempt` outcome. Every tool
provider implements `execute(ToolCall) -> ToolAttemptOutcome` and receives
sealed, controller-free `AttemptContext`. A body has no durable
process administration or recursive batch dispatch. If it needs a result later,
it returns Pending; if it needs to cause durable work, it returns an intent.
ADR 0116 defines this tool interface.

`ToolCall` carries one immutable manifest. The dispatcher owns admission and
routing. `ToolCallId` is mandatory Lash identity for the admitted logical call;
provider call ids are correlation only. Crash recovery and reported-failure
retries preserve `ToolCallId`, while `attempt_number` identifies the attempt
(ADR 0117). Authors key external idempotency on the logical call id.

`Done` carries terminal output and ordered `ToolIntents`. `Pending` cannot
carry general intents. It may carry a `PendingAnnouncement`, appended by the
runtime at park time, or exactly one same-session `DeclaredStart` through its
pending resolver. The Run records the declaration's recoverable launch obligation and its
pre-admission cancellation decision. A non-final attempt's
declarations are discarded.

A direct model completion inside the attempt runs locally as part of its
opaque work, with ordinary request planning and outcome bookkeeping. It does
not admit a nested durable execution. Multi-step durable composition
belongs in an explicit process body.

The runtime records the final attempt before realizing its declarations.
Within that attempt, declarations are admitted in source order. Realization
uses recorded payload and stable admitted identity. It does not reread live tool
visibility or host configuration as a new admission gate. Unknown, terminal
and conflicting command outcomes remain typed recorded outcomes.

Hosts can submit typed declarations through `ToolIntentIngress`, bound to a
session and execution scope. Identity validation happens before realization.
Committed outcome rows answer recovery; an external submission is not
automatically an idempotency lookup. Child lifetime and scope-end settlement are registration
facts governed by ADRs 0094 and 0108.

Standard `batch` is protocol sugar expanded into the turn's admitted tool round. `spawn_agent` is an ordinary opaque tool returning Pending with a
`DeclaredStart`. There is one tool execution route, with no separate
orchestrating body class.

## The protected phase, and where coordination runs

The phase between a final attempt record and completion of its declarations
and projection is protected. Final recording and cancellation compete at one
durable fenced decision point (ADR 0099):

- A final record that wins retains settlement ownership. Recovery finishes its
  declarations and projection before the scope reports success.
- A cancellation decision that wins refuses subsequent final recording typed,
  without writing an outcome. Cooperative signalling and grace cannot reverse it.
- Worker loss does not cancel a recorded declaration. Cancellation does not
  undo admitted commands or destroy retained descendant obligations.

Cross-call protected-drain admission follows recorded final-commit order. It does not
wait for a sibling that has not committed, but an earlier committed drain can
hold a later drain until it finishes. This is distinct from source order
inside one attempt.

Retries, completion-key derivation, Deferred subscription, final decision and
protected drain run in the owning Run coordinator. Only the opaque body runs
inside X. Each attempt records its own result; resume of a committed result
runs no body. The admitted executable and prepared input supply recovery, so a
caller does not reconstruct an independent child handler.

## Consequences

Resume of a committed outcome returns it without invoking the body again. A
crash after the started row commits but before the outcome commits follows
the call's `ExecutionPolicy` (ADR 0132 §5 and §7): a `Once` attempt records
`Interrupted` and never runs again; a `Repeatable` attempt runs again at the
same ordinal, so its external writes and model work can repeat. A reported
failure retries only as the recorded policy admits (ADR 0110).

Tool-body memos, an action ledger and a body-level replay opt-in are rejected
because they create another durability contract inside opaque code. Authors make external
writes idempotent when needed and place independent durable boundaries in
process steps. World readiness, such as artifact loading, belongs in process
preparation or execution; it is not a store-dependent start-admission verdict.

## Implementation

- [Opaque provider and attempt context](../../crates/lash-core-execution/src/tool_provider.rs) and [exclusive outcome variants](../../crates/lash-core-execution/src/tool_intent.rs).
- [Prepared atomic attempt runner](../../crates/lash-core-execution/src/tool_dispatch/atomic_attempt.rs). Run callers share validation, attempt-local completion and capture buffers, and body execution. The runner consumes the admitted prepared call and issues no coordination commands.
- [Pending declarations](../../crates/lash-core-execution/src/tool_result.rs) and [pending launch](../../crates/lash-core-execution/src/tool_dispatch/pending_resolver.rs).
- [Final recording and intent drain](../../crates/lash-core-execution/src/tool_dispatch/run_coordinator/drain.rs).
- [Run final-or-cancel arbitration](../../crates/lash-core-store/src/tool_run/run_event.rs).
