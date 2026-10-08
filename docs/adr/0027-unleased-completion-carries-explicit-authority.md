# 0027: Process completion carries explicit authority

## Status

Accepted.

## Decision

`ProcessRegistry::complete_process` requires `ProcessCompletionAuthority`. The backend validates it inside the terminal transaction and records the accepted authority in event evidence. Every process is executed by its engine kind; there is no externally owned input class. External work is awaited by an engine's `AwaitExternal` action ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §10).

Completion authority belongs to the runtime, not the bearer of a completion
key. A host resolves a parked call; the process engine consumes that result
and decides its typed terminal under ADR 0136. Resolution does not authorize
`complete_process`.

The authority names who writes the terminal:

- the process actor's owner, whose terminal transaction is an `ActorTx` fenced by the epoch of its claim (ADR 0132 §3);
- a cancellation that ends the process without running its engine: a claimer of a waiting or parked process, or lash after the engine's cancel grace, commits the forced `Cancelled` terminal from registry state (ADR 0132 §10 and §11).

There is no default authority. Authority validation precedes terminal idempotency. Valid repetition returns the retained outcome and original authority without applying a prelude, adding events or waking waiters again. An equal proposal is `AlreadyApplied`; a different proposal reports that the retained terminal already owns the result.

Process execution writes carry the same epoch fence. A writer whose epoch is stale is refused with `OwnershipLost`, rolls back and drops the actor. The registry has no execution lease, renewal or takeover API beyond the actor claim of ADR 0132 §3.

## Why and consequences

Caller convention alone cannot establish who may write a terminal. The required argument, uniform transactional validation and retained evidence make that discipline inspectable. In-process tokens are not an auth boundary; the embedding host owns security policy. SQLite file, SQLite memory and PostgreSQL enforce the same rule.

Terminal construction shares `terminal_append_request` helpers. [Completion authority](../../crates/lash-core-execution/src/runtime/process/events.rs), [invocation authority](../../crates/lash-core-execution/src/runtime/process/model/execution.rs) and [registry transitions](../../crates/lash-core-execution/src/runtime/process/registry_transitions.rs) define the contract.

[ADR 0136](0136-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
