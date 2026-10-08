# Durable waits are scoped wait rows with a first winner

## Decision

Turns and Runtime Processes use the same durable wait: a wait row in the lash
store under [ADR 0132](0132-durability-is-state-first-over-the-lash-store.md)
§6. A wait has a kind, an owner actor, an owner scope for revocation and an optional
deadline written once at creation. Execution Scope provides wait, cancellation
and trace identity; its bound Execution Environment supplies execution
requirements. Process joins, tool completions, custom host waits and timers are
wait kinds of this one mechanism. The host controls ingress authorization, as
described by ADR 0014 and ADR 0046.

A waiting turn remains the session's active turn and commits no partial
transcript merely because it waits. A waiting process records its wait as an
observation of running work. Process events describe the wait; they do not
resolve it. An actor with nothing runnable commits its last phase and releases
as `waiting`; it holds no node while it waits.

## Rules and guarantees

The row exists from minting, so a resolution that arrives before the owner
awaits finds it. Resolution is a conditional update from `pending`; the first
terminal resolution wins and is retained. A repeated resolution with the same
digest answers `AlreadyResolved`, one with a different digest answers
`Conflict`, a revoked or timed-out wait answers `Revoked` and a key that names
no wait answers `Unknown`, distinct from runtime failures. A deadline that passes resolves the wait
`TimedOut { WaitDeadline }` before anything acts on it.

A host-resolvable key is its wait's id, 128 random bits: a bearer capability
that carries no scope or kind. Lash keeps no completion secret; the host
authorizes who may resolve and hands keys only to those callers. Hosts resolve only the
`tool_completion` and `custom` kinds; turn cancellation and process
terminals have their own admission paths. Host events resolve deferring calls
under ADR 0136. Waiting is a facet on a running process, mirrored
by wait and resume events, rather than a lifecycle status.

## Alternatives and consequences

Making a suspended turn a process is rejected because a session-owned turn
must not acquire process addressability and lifecycle. Requiring authors to
start a process before a long tool call is rejected because suspension is an
execution concern. Adding a separate primitive per wait kind is rejected
because each kind needs only a row with its own resolution path. Sharing the
wait mechanism preserves those separate ownership rules.

The current implementation is in
[durable wait identities](../../crates/lash-core-store/src/await_event_identity.rs);
the substrate lanes implement wait rows under ADR 0132 §6.

[ADR 0136](0136-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
