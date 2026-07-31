# Process observation is best-effort push over state truth

## Status

accepted

## Decision

Process event observation has two tiers with different guarantees, and hosts
must not confuse them:

- The durable event log (`ProcessRegistry::events_after`) is the **truth**. It
  is the complete, ordered, crash-durable record of a process's events.
- A `ProcessEventSink` is **best-effort freshness**, never truth. When a host
  installs a sink, the `WatchedProcessRegistry` decorator calls `sink.emit(...)`
  after each successful `append_event`, in that pod's per-process append order.
  There is no buffering, no retry, and no delivery guarantee across pod crashes
  or restarts: an event written durably may never reach the sink. Consumers that
  need completeness reconcile from the event log — typically at terminal time.

Two consequences are load-bearing:

- **The sink emits every appended event, terminals included.** *(Superseded on
  this point by ADR 0046: emission is uniform across explicit appends, lifecycle
  verbs, and terminal completion, so projections never silently miss a
  transition. This ADR originally excluded terminal events from the sink.)*
  Terminal *observation* still rides `ProcessWorkDriver::await_terminal`
  (ADR 0016), which reads the durable terminal state — engine-native (Restate
  ingress attach) where available, the in-process change hub plus backoff point
  reads otherwise. The sink stays best-effort freshness: hosts must not wait on
  it for completion, and a terminal seen on the sink is a hint, not delivery.
- **Emission cannot fail or slow-fail the write.** `emit` returns `()`, so a
  sink can never fail an append; the durable write has already committed when
  `emit` runs. But the decorator awaits `emit` inline, so a slow sink slows
  every append. Sink implementations must return fast and offload any I/O to a
  channel or background task.

The sink is installed once, at the point the decorator is wrapped —
`watch_process_registry_with_sink`, threaded through the three wrap funnels:
`ProcessWorkDriver::new_with_sink` (bare callers), `RestateProcessDeployment::new_with_sink`
(durable hosts), and `LashCoreBuilder::process_event_sink` (the facade's inline
registry path). Stores stay pure state: nothing sink-related touches the
`ProcessRegistry` implementations or the store crates.

Retention is an explicit host lever, not an automatic policy.
`ProcessRegistry::prune_terminal_processes(cutoff_epoch_ms, filter, watermark)`
physically deletes
terminal process rows older than the cutoff — together with their events, wake
acks, observer edges, and lease rows — and never touches non-terminal rows. It
returns a `ProcessPruneReport { pruned_processes, pruned_events }`.

## Why

Hosts want prompt, low-latency visibility into a running process's events —
render a live log, forward events to their own store — without polling
`events_after` on a timer. A push feed serves that. But making the push feed a
source of truth would force it to carry buffering, retries, and crash-recovery
guarantees, re-creating the durable log badly. The event log already *is* the
durable record with those properties; the sink should be a cheap freshness
overlay on top of it and nothing more. Splitting the two keeps each honest: the
log guarantees completeness, the sink guarantees only promptness-when-it-arrives.

Terminal observation deliberately does not ride the sink. ADR 0016 already put
terminal waits on the work-driver seam, where the mechanism (engine promise vs.
in-process watch) matches the deployment. Routing completion through a
best-effort sink would reintroduce a lost-wakeup surface that the await seam was
built to eliminate. So `complete_process` writes its terminal event past the
decorator, and the sink is confined to non-terminal appends.

The prune lever exists because a host that projects process results and events
into its own store becomes the real consumer of that data; the lash registry
rows then have no remaining reader and would grow without bound. Only the host
knows its retention window and when a terminal process's data is safe to drop,
so retention is a host-scheduled call rather than an automatic sweep inside the
registry. Making `prune_terminal_processes` a required trait method (no default)
compile-forces every store to implement deletion deliberately on its next bump,
rather than silently inheriting a no-op.

## Consequences

- `ProcessEventSink` is optional and absent by default; deployments that do not
  install one see no behavior change. The decorator still publishes in-process
  change ticks for the awaiter exactly as before, sink or no sink.
- Sink consumers must treat a gap as expected and reconcile from `events_after`.
  A consumer that needs the terminal outcome awaits it via the work driver, not
  the sink.
- `prune_terminal_processes` is a required `ProcessRegistry` method. New and
  downstream registry implementations must implement physical deletion across
  the process/event/wake-delivery/observer/lease rows in one transaction. The
  `WatchedProcessRegistry` decorator delegates it without a hub bump, since
  pruned rows are terminal and their waiters resolved long ago.
- Callers of `prune_terminal_processes` own correctness of the retention window:
  a retained tombstone produces a typed no-longer-retained result rather than an
  unknown-process result. A cutoff shorter than a live waiter's lifetime can
  prune a process id out from under a late await, which then returns the typed
  no-longer-retained information. After tombstone compaction, the same id is
  indistinguishable from an unknown process. The retention and compaction
  windows must be comfortably longer than any await.
