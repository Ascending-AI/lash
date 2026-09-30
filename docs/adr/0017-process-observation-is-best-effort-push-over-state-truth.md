# Process observation is best-effort push over state truth

## Status

accepted

## Decision

The retained durable process event log, read through `ProcessRegistry::event_page`, is ordered state truth. `ProcessEventSink` is optional best-effort freshness. `WatchedProcessRegistry` emits events after successful writes, including lifecycle and terminal events, in per-process append order. Batch events reach sinks in their committed order.

The decorator supplies no durable buffer or retry guarantee. Pod failure can leave a committed event undelivered, so consumers requiring completeness reconcile event pages or the record change feed under ADR 0020. Terminal waiting uses the work-driver contract under ADR 0016.

## Rules and guarantees

`emit` returns `()`: a sink cannot fail or roll back the committed write. The decorator awaits emission inline, so sink implementations must return promptly and offload I/O. Observers attach to the shared watched registry through `add_event_sink`. A `ProcessEventSinkRegistration` detaches its sink when dropped. Deployment wrapping can also provide an initial sink.

Retention is host-scheduled through `prune_terminal_processes(cutoff, filter, watermark)` under ADR 0023. It removes eligible retired rows and events and retains typed tombstone evidence. The facade coordinates cross-store trigger cleanup. A late read can report `ProcessNoLongerRetained`; after tombstone compaction the id can be unknown. Host retention windows must cover still-replayable waiters.

## Why and consequences

A durable push feed is rejected because it duplicates the durable log's buffering and recovery responsibilities. Routing terminal waits through the sink is rejected because missed delivery must not strand a waiter. Stores stay state interfaces; decorators add freshness and local change ticks. Sink lifetime follows the observer registration, while retained state remains available independently of that observer.

[Watched registry and registrations](../../crates/lash-core-execution/src/runtime/process/awaiter.rs), [event emission](../../crates/lash-core-execution/src/runtime/process/awaiter/registry_support.rs) and [retention contract](../../crates/lash-core-execution/src/runtime/process/registry_concerns.rs) implement the split.
