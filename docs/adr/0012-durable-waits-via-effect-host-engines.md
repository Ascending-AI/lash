# Durable waits use effect-host engines and engine-owned journals

## Decision

Long-lived work waits through one durable one-shot keyed promise, `AwaitEvent`, and its resolve operation. Named signals, process joins and timer wakes compile to that wait mechanism. Run-owned aggregates compose admitted effects and timers under ADR 0065. Deferred tools use an authenticated immutable source seal with short subscriptions (K4); those descriptors remain pending until the source resolves or cancellation seals it.

Lash owns the effect-controller contract. Restate supplies the execution journal and durable suspension under ADR 0104. SQLite and PostgreSQL store domain state. Settled session history and effect replay are separate responsibilities joined by stable operation identity; the process event log supplies observation rather than a second replay journal.

## Restate deadline and cancellation replay shape

A deadline-bearing wait records one absolute Unix-epoch deadline before calling `LashDurableWaitWorkflow/await_resolution`. Replay reuses it and computes remaining time, preserving one total budget and the nested call payload. The deadline has an explicit wire version and incompatible payloads are refused. A no-deadline request carries no deadline field.

`observe_turn_cancel` chooses between a wait and a wait-plus-gate command sequence. The choice must be stable for the invocation. Callers deriving it from stable turn scope need no separate journal step; a caller with a variable choice must record it before choosing the command shape.

## Alternatives and consequences

Using the process event log as an effect journal is rejected because it duplicates engine replay and couples observation to one recovery implementation. Adding `AwaitSignal` and `AwaitProcessTerminal` primitives is rejected because each would expand the engine contract without adding a mechanism beyond a keyed promise.

Named signals carry declared schemas and validate their payloads. Waiting is a facet on a running process, mirrored by wait/resume events, rather than a lifecycle status. [Deadline construction](../../crates/lash-restate/src/controller/context.rs), [durable promises](../../crates/lash-restate/src/durable_wait.rs) and [cancellation request shape](../../crates/lash-restate/src/controller/turn_cancel_request.rs) implement replay-sensitive wait behavior.
