# Process observation is best-effort push over state truth

## Status

accepted

## Decision

The retained durable process event log, read through `ProcessRegistry::event_page`, is ordered state truth. `ProcessEventSink` is optional freshness, delivered at least once from a publication mark (below). `WatchedProcessRegistry` emits events after successful writes, including lifecycle and terminal events, in per-process append order. Batch events reach sinks in their committed order.

The decorator supplies no durable buffer or retry of a sink call. Consumers requiring completeness reconcile event pages or the record change feed under ADR 0020. Terminal waiting uses the work-driver contract under ADR 0016.

## Publication mark (FIG-5396)

A durable process's events reach host sinks at least once. The process row carries a publication mark, the last sequence its owners' nodes handed to their sinks; each transition an owner commits, and its end, moves it to what its node emitted, and a node that takes the process over publishes from it, not from the log's end, so events an owner committed and died before publishing are delivered by its successor. On one node every path that emits a process's events (a registry append and the owner's publication) shares one forward-only mark, which a terminal does not drop, so no sink of that node hears an event twice. A takeover can hand again what its predecessor published and did not record yet, and an append through another node's registry also reaches that node's sinks; each event's `(process_id, sequence)` is its stable identity, and a consumer that must act once per event dedupes on it.

## Durable-before-push law

A turn, session fault or process terminal commits its durable record before
any best-effort observation publishes that terminal. A failed transaction
publishes no terminal. Publication failure cannot undo or fail a committed
write. A reconnecting host reconciles the durable cursor feed under ADR 0020
when live replay reports a gap.

## Rules and guarantees

`emit` returns `()`: a sink cannot fail or roll back the committed write. The decorator awaits emission inline, so sink implementations must return promptly and offload I/O. Observers attach to the shared watched registry through `add_event_sink`. A `ProcessEventSinkRegistration` detaches its sink when dropped. Deployment wrapping can also provide an initial sink.

Retention is host-scheduled through `prune_terminal_processes(cutoff, filter, watermark)` under ADR 0023. It removes eligible retired rows and events and retains typed tombstone evidence. The facade coordinates cross-store trigger cleanup. A late read can report `ProcessNoLongerRetained`; after tombstone compaction the id can be unknown. Host retention windows must cover every reader that still awaits a retained process.

## Why and consequences

A durable push feed is rejected because it duplicates the durable log's buffering and recovery responsibilities. Routing terminal waits through the sink is rejected because missed delivery must not strand a waiter. Stores stay state interfaces; decorators add freshness and local change ticks. Sink lifetime follows the observer registration, while retained state remains available independently of that observer.

[Watched registry and registrations](../../crates/lash-core-execution/src/runtime/process/awaiter.rs), [event emission](../../crates/lash-core-execution/src/runtime/process/awaiter/registry_support.rs) and [retention contract](../../crates/lash-core-execution/src/runtime/process/registry_concerns.rs) implement the split.
