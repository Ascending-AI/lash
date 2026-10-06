# 0110: The engine owns process recovery; lash never re-runs started work

## Status

Accepted. The durable mechanics are owned by
[ADR 0132](0132-durability-is-state-first-over-the-lash-store.md): recovery
loads committed state, and phase rows record `Once` and `Repeatable`
executions.

## Context

Re-executing a started process from scratch can repeat effects whose outcomes
the process never committed. The durable engine owns resume and retry policy.
The registry records execution facts and terminal evidence; it cannot
fabricate an outcome for work whose outcome never committed.

## Decision

### 1. Lash executes every process it registers

Every process input is work the engine executes: an engine input runs on the
host-registered `ProcessEngine` of its kind, and a session turn runs its
child session. Every registration captures the execution environment it runs
under and inserts the process's runnable actor in its start transaction
(ADR 0132 §12). Registration carries no recovery disposition and no ownership
class.

Work a host runs outside lash is a process of a host-registered engine that
awaits that work's completion durably, through its `AwaitExternal` action: a
wait row with a completion key and a deadline (ADR 0132 §6 and §10). Its
deadline, cancellation and recovery are the process's own, so there is no
second kind of row that lash never runs and only its host can close.

Evidence: `crates/lash-core-execution/src/runtime/process/validation.rs`.

### 2. Recovery loads committed state

A started process resumes from committed state. Its actor's last committed
phase or VM snapshot is the state, and a node that claims the actor loads it
and continues (ADR 0132 §2 and §3). An uncommitted stretch is recomputed from
that state.

A process whose state its node cannot decode is never claimed by that node
under the claim filter of
[ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md) §1. A VM
continuation bound to an executable identity whose bytecode contract changed
ends `Abandoned` with `ResumeRefused`. Executable artifact corruption ends with
`StoredArtifactCorrupt`, carrying the artifact reference and typed validation
cause.

The registry refuses an execution write whose epoch is stale. A process's
terminal transaction resolves every `process_terminal` wait on it, so waiters
receive the terminal in that commit (ADR 0132 §11).

Evidence: `crates/lash-core-execution/src/runtime/process/validation.rs:146`.

### 3. Effect implementors own the window before an outcome commits

An effect can finish before its outcome commits. What happens next is the
execution's recorded `ExecutionPolicy` (ADR 0132 §5 and §7): a `Once`
execution started without an outcome records `Interrupted` and never runs
again; a `Repeatable` execution runs again at its same ordinal. An implementor
of a `Repeatable` effect makes the repeat safe using Lash's `ToolCallId`. Lash
derives and records that id for the admitted logical call; it is stable across
`Repeatable` ordinals and reported-failure retries. Provider call ids are
correlation data. [ADR 0117](0117-lash-names-every-tool-call.md) owns the
derivation and recording rules.

`ExecutionPolicy::Once` disables retry after a reported failure. A
process-engine implementor performs effects only as admitted `Step` actions,
each with its own execution policy, and its `advance` function performs none.

Evidence: `crates/lash-sansio/src/tool_call_id.rs:270`,
`crates/lash-core-execution/src/tool_provider.rs:1011`, `:1387`, and
`crates/lash-core-execution/src/tool_dispatch/attempt_coordinator.rs:48`.

### 4. The engine bounds retries

Lash carries no process retry budget in registrations or start requests beyond
the execution policy. A `Repeatable` execution records its `BoundedRetry`, and
a retry is a record with a due time. An actor whose claims make no phase
progress counts failed activations and parks with `ActivationLoop` at its
activation budget, writing no terminal (ADR 0132 §3). Resuming that park
claims the actor again from its committed state.

The two deferred-work kinds have their own attempt policy, owned by ADR 0109.

Evidence: `crates/lash-core-execution/src/runtime/process/model/start_request.rs`.

### 5. Who writes `Abandoned`

`AbandonWriter` is `Producer` or `ResumeRefused { reason }`. The producer
records its own lost-work outcome. Lash writes a resume refusal when it
cannot safely continue the execution.

`ProcessCompletionAuthority` names the actor's owner, fenced by its epoch, or
a cancellation that ends the process without running its engine
([ADR 0027](0027-unleased-completion-carries-explicit-authority.md)). A stale
owner cannot end a process a later owner carries.

Evidence: `crates/lash-core-execution/src/runtime/process/events.rs:71`
and `crates/lash-core-execution/src/runtime/process/events.rs:130`.

### 6. Operators use cancellation

An operator stops a process through cancellation. A process no node can resume
ends through the engine's resume refusal. A host engine whose external work was
lost ends its process with its own `Abandoned { Producer }` outcome; a host
resolves external work that completed by resolving its wait with the
completion key.

These operations record the outcome directly. There is no separate abandon
request waiting for a Lash lease to expire, and closing a host does not
abandon started work.

Evidence: `crates/lash-core-execution/src/runtime/process/registry_concerns.rs:511`
and `crates/lash-core-execution/src/runtime/process/events.rs:130`.

### 7. The durable engine owns process execution

Lash's durable engine owns process execution and recovery over the lash store
(ADR 0132 §1). Process facts, continuation data and engine state are rows in
that store. Laws run the production runtime over a fault-injecting store with
labelled commits, a virtual clock and `SimNodes` (ADR 0132 §14).

### 8. Cancellation is cooperative; hard isolation is the host engine's

Lash provides the `ProcessEngine` seam, an `advance` state machine (ADR 0132
§10). A host registers its own engines in its own deployment, and its nodes
claim each process's actor the same way they claim sessions. An isolated tool
binds a host-registered engine through `IsolatedProcessBinding` and runs as
one process of it.

Lash's cancellation of a process is cooperative: it writes the request to the
process's mailbox, the running step sees its token, and after the engine's
cancel grace Lash commits a forced `Cancelled` terminal and drops the step.
Lash ships no engine that executes OS programs or keeps ownership state on
local disk. A host that needs hard isolation (an OS kill and reap, adoption of
a live worker across host loss) builds it into its own `ProcessEngine`,
together with whatever durable ownership that requires.

Evidence: `crates/lash/src/tests/isolated_tool_route.rs`.

## Consequences

A registry row cannot authorize re-execution from scratch. A started `Once`
execution without an outcome yields an explicit `Interrupted` instead of a
second run. Host-run external work has one path, a host engine process, so it
gets the same deadline, cancellation and recovery as every other process.

A per-process recovery disposition would offer different answers to the same
missing-outcome problem. The execution policy, recorded before the work
starts, and the implementor's call-id idempotency cover that boundary.
