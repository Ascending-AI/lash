# 0136: The host owns events, routing and scheduling

## Status

Accepted.

## Context

D-HOSTEVENTS puts product events, their routing and their schedules in the
host. Lash supplies durable calls, process execution and lifecycle facts.
Approvals, outside callbacks, scheduled triggers and process-end notices
need crash-safe delivery without making the runtime interpret product events.

## Decision

### 1. Hosts compose durable primitives

A host tool that can defer obtains its key from
`AttemptContext::completion_key()`, records it in host storage and returns
`Pending`. Admission creates the wait before the body
runs, so a completion arriving before the call parks still finds its row.
`Completions::resolve` uses first-writer settlement: an equal repeat answers
`AlreadyResolved`, a different result answers `Conflict`, and a revoked or
expired wait answers `Revoked`. Keys are bearer capabilities; the host
authenticates and authorizes the caller before resolving on its behalf.

Every tool manifest declares its host-set `execution: Duration` body bound.
A deferring tool also declares `park: ParkBound`, either `Within(Duration)`
or `UntilScopeEnd`. A non-deferring tool declares no park bound. Tool
definitions set these with `with_execution(Duration)` and
`with_park(ParkBound)`. Registration refuses missing bounds as `MissingBound`
and a park without deferral as `ParkWithoutDeferral`; admission also checks
ungated manifests. Body and park bounds are separate,
and Lash supplies no default for either. Admission fixes the park deadline
once; retries and takeover preserve it. `UntilScopeEnd` has no deadline and
is revoked when its owning turn or process scope ends. Engine `PinKey` and
`AwaitProcess` actions likewise carry `bound: ParkBound`,
with no runtime default or ceiling. Engine steps declare their body bound
through `EngineSteps::execution` or `EngineHostSteps::execution`.

`AttemptContext::call_id()` names one logical call across redelivery and
retry ([ADR 0117](0117-lash-names-every-tool-call.md)). Hosts key their records
and external effects on that id. Caller context provides `owner`,
`enclosing_process`, `logical_run` and `process_spawn_provenance`; provenance
is context, not authorization or an instruction to notify a parent.

`Completions::parked(owner)` accepts `CallOwner::Session` or
`CallOwner::Process` and returns pending `ParkedCall` records with `key`,
`owner`, `call_id`, `tool_id` and `deadline`. It is a snapshot read derived
from pending waits and admitted calls, useful for reconciliation after a
restart. A key can settle or be revoked immediately after that read.
Lifecycle waiting and resumed facts carry `WaitKind::Call { call_id, tool_id }`
in their wait descriptor, never the key.

`session.send(input).id(TurnId)` admits idempotent turn input. The engine
owns its drive and continuation; hosts send and observe its handle.
`ProcessStartRequest::with_host_start_key` makes process starts idempotent
while the process is retained. The start-args check validates a host's input
mapping against the definition's authoritative signature, in partial mode at
registration and complete mode before start. Checking arguments does not
register a process or pin its definition.

### 2. Lifecycle facts are a closed vocabulary

The process log records typed lifecycle facts: started, waiting with
`call_id` and `tool_id`, resumed, effect outcome, effect omissions, cancel
requested, observer added, observer removed, external reference set, and the
four terminals: succeeded, failed, cancelled and abandoned. Terminals carry
typed engine outcomes. The log has no producer-defined event types, schema
selectors or product payload interpretation. A process's registry projection
and lifecycle event commit together
([ADR 0046](0046-process-transitions-are-events-record-is-a-fold.md)).

Hosts reconcile `processes_changed_since`, `turns_changed_since` and
per-process event pages under
[ADR 0020](0020-process-change-feed-is-a-record-cursor-read.md). Best-effort
push is freshness; retained records and cursor reads are completeness.
A host acknowledges a cursor only after recording its page or completing its
idempotent deliveries, and respects projection watermarks and history release.

### 3. Delivery and retention are host contracts

Delivery from the host comes after the commit, deduplicated by keys. A host
records an event, decision or due tick in its own durable transaction, then
sends, starts or resolves through Lash. A retry reconciles unfinished delivery
with the same key and content. Lash's internal mailbox write and actor wake
remain atomic; the host's product record and its Lash delivery are separate
commits. A successful delivery followed by a lost host acknowledgement must
be safe to repeat. Conflicting content is a typed refusal, not a new action.

Dedup after pruning is a host contract. A start key deduplicates only while
its process is retained; the host does not prune a process before it has
recorded that process's binding. Once recorded, the host answers a duplicate
from its own record rather than starting again. Lash has no receipt table for
host event delivery. Hosts retain send identity evidence and product records
for as long as their sources can redeliver, and coordinate their own retention
with [ADR 0023](0023-retention-stays-a-parameterized-host-lever.md).

There is no environment capture for triggered processes. The host chooses
the definition, arguments, lifetime and tools for each start. A saved
registration holds its definition with an explicit host artifact pin under
[ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md); a copied
id alone retains no bytes. No registering agent's environment is inherited.

Internal actor wakes, generic queued work, `lash_durable::NodeWakes` and
`node_wakes()` remain runtime primitives. Cross-node `pg_notify` hints affect
latency, not product routing. SQL `TRIGGER` statements and the language's
generic `yield` rejection are independent of host event delivery.

### 4. Host patterns

**Approvals.** An approval tool declares a short body bound and an
`UntilScopeEnd` park, records its completion key and caller context under
`call_id`, then returns `Pending`. The host commits the authenticated human
decision before resolving the key. After a crash it reconciles the decision
and retries that resolution. `AlreadyResolved` is success; `Conflict` is a
conflicting decision; `Revoked` means the call cannot receive it.

**Triggers.** The host owns registrations, enabled state, source provisioning,
input mappings and timers. It checks a mapping's partial start arguments at
registration and complete arguments at delivery. A durable occurrence or
scheduled tick has a stable host identity. After committing it, the host
starts with `with_host_start_key`, records the returned process before pruning
is possible, and sends any session notification under a stable `TurnId`.
Redelivery consults that record. The host explicitly supplies the started
process's tools and keeps its definition pinned while the registration needs it.

**Process-end notices.** The host consumes committed process lifecycle facts,
records the destination and notice identity, then calls `send().id` on the
selected session. Re-reading a cursor repeats the same send. It advances its
cursor only when the notice is recorded safely; a disconnected observer
reconciles the retained feed. A parent terminal does not prove all descendants
are quiescent; a host requiring that fact reads `live_until_descendants`.

The [workbench](../../examples/agent-workbench/src/approvals.rs) shows approvals;
its [host routes and timers](../../examples/agent-workbench/src/main_sections/)
show triggers and notices. These are host code, built on the same facade as any
other application.

## Consequences

Lash owns execution, call settlement and lifecycle truth. Hosts own product
buffers, event order, schedules, routing, authorization and delivery retention.
A host can choose FIFO or another product order without changing process
execution. Cross-commit delivery requires a durable host record and keyed
reconciliation; a best-effort callback alone cannot guarantee completeness.
Putting product routing in the runtime would create a second authority for
events and retention. The durable primitives give hosts those decisions
without an event interpreter in core.
