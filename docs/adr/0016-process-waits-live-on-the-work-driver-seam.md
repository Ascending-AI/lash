# Process waits live on the work-driver seam

## Status

accepted

## Decision

`ProcessRegistry` stores process state, observer relationships, wake bookkeeping and events. It exposes point reads and mutations rather than backend-specific wait methods. Coordination belongs to `ProcessWorkSubstrate::await_process_terminal`, returning `ProcessTerminalWait::{Terminal, Reattach}`.

Inside an actor, a process await is a bounded `process_terminal` wait row ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §6 and §11). The awaited process's terminal transaction resolves it and wakes the waiter, so the waiting actor holds no node. The wait races the awaiter's own cancel mail, so a cycle of waits is cancellable.

A host caller that awaits a process outside any actor uses `ProcessRegistryAwaiter`, which reads `get_process` and `event_page`. A watched registry's `ProcessChangeHub` supplies local notifications; backoff handles mutations committed by another node. Default polling starts at 25 milliseconds, doubles, and caps at one second.

## Terminal-wait failures

A retained terminal outcome is returned from durable state. A store fault while reading or arming the wait is transient and returns `Reattach` without writing a process outcome; reattachment observes the same durable process. Typed refusals and local encoding errors remain errors.

## Why and consequences

Per-backend wait loops duplicate lost-wakeup handling and force stores to know the deployment's execution economics. They are rejected. The work driver chooses a durable wait row or shared point-read coordination. Watched decorators can add local notifications without changing storage traits, and sinks under ADR 0017 remain optional freshness rather than terminal authority.

[Shared awaiter](../../crates/lash-core-execution/src/runtime/work/awaiter.rs) and [cadence](../../crates/lash-core-execution/src/runtime/work/cadence.rs) implement the host-side read; the substrate lanes implement `process_terminal` wait rows under ADR 0132 §11.
