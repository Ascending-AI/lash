# Lifecycle events and host waits

Lash records a closed vocabulary of lifecycle facts. Hosts own product events,
routing and scheduling under
[ADR 0137](adr/0137-the-host-owns-events-routing-and-scheduling.md). Approvals,
callbacks and triggers are host patterns built on durable calls, keyed sends
and starts, and lifecycle cursors.

`LashCore::completions().parked(owner)` lists a session's or process's pending
admitted tool calls. `lash::admin::CallOwner` selects `Session(SessionId)` or
`Process(ProcessId)`. Each `ParkedCall` carries its completion `key`, `owner`,
stable `call_id`, `tool_id` and `deadline`, read from the owner's pending wait
rows. This is a snapshot: a returned key may be settled concurrently.

`LashCore::completions().pinned_keys(process)` lists the pending keys a
process's engine pinned with `PinKey`: each `PinnedEngineKey` carries its `key`,
`process`, the engine's `name` for it and `deadline`. It reads the same wait
rows, so any node answers, before and after a restart or a handover.

A process that releases to wait records one `process.waiting` fact for each
thing it is blocked on, and its record lists them all
(`ProcessLifecycleState::Waiting { waits }`, oldest first):

- a deferred call: `WaitKind::Call { call_id, tool_id }`;
- a key its engine pinned: `WaitKind::Key { name }`;
- a sleep: `WaitKind::Sleep { until_ms }`;
- another process's terminal: `WaitKind::Process { process_id }`.

Each wait carries `since_ms` and, when the engine named one, the `site`
(`node_id`, `occurrence`) of the node that waits. None carries a bearer key or
a wait id. The end of each wait records `process.resumed`; the process reads
`running` again once no wait is left.

A park is not a wait. `ObservedProcess::park` carries the actor's
`ProcessParkReason` beside the lifecycle while the process is parked, and the
lifecycle keeps what the record says.

`resolve(key, resolution)` settles a wait, first writer wins.
A second resolution answers `AlreadyResolved` (same
digest) or `Conflict` (another digest). A key that names no wait answers
`Unknown`; one whose wait was revoked or timed out answers `Revoked`. A key of
any other kind answers `ReservedKind`. None of them writes anything.
A deferring tool has a host-set execution bound and a separate park bound:
`Within(Duration)` or `UntilScopeEnd`. Admission records the park deadline once;
a crash or takeover changes neither the deadline nor the call identity.
An `UntilScopeEnd` wait has no deadline and is revoked when its scope ends.
Lash supplies no default for either tool bound.

The tool obtains its completion key from `AttemptContext::completion_key()`
and records it with `call_id` and caller context before returning `Pending`.
The wait exists before the body runs. Hosts list pending calls through
`Completions::parked(CallOwner::Session(..))` or
`Completions::parked(CallOwner::Process(..))`. Each record carries its key,
owner, call id, tool id and deadline; lifecycle waiting facts expose the call
and tool, never the key.

A completion key is a bearer capability. Authenticate and authorize a caller
before resolving on its behalf. `Completions::resolve(key, resolution)` is
first writer wins: `AlreadyResolved` is an equal repeat, `Conflict` a different
result, `Revoked` a timed-out or revoked wait, `Unknown` an absent key, and
`ReservedKind` a runtime-owned wait. The latter answers write nothing.
A host may resolve tool-completion or custom waits, not a process terminal.
The process engine consumes the completion and owns its terminal decision.

Delivery from the host comes after the commit, deduplicated by keys. An
approval records the authenticated decision before resolving; a trigger records
its occurrence before starting; a process-end notice reads a committed
lifecycle fact before sending. Unfinished delivery is reconciled after a
restart with the same content and identity. Retain a keyed start's process
until its binding is recorded; after pruning, the host's own record answers a
duplicate.

Read `processes().changed_since`, `turns_changed_since` and per-process event
pages for completeness. Advance a cursor after recording its page or completing
its keyed deliveries. Respect projection watermarks and typed history gaps.
Best-effort push improves freshness but cannot authorize skipping reconciliation.
The [hosting guide](operations/durable-hosting.md#9-events-routing-and-scheduling)
and [workbench](../examples/agent-workbench/src/approvals.rs) show the host patterns.
