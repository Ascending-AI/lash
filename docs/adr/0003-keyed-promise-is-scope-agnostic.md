# Durable waits are scoped and resolved by the effect host

## Decision

Turns and Runtime Processes use the same one-shot keyed promise: `AwaitEvent { key }` and the effect host's resolve operation. Execution Scope provides replay, wait, cancellation and trace identity; its bound Execution Environment supplies execution requirements. A wait key identifies a promise. The host controls ingress authorization, as described by ADR 0014 and ADR 0046.

Restate owns durable suspension and promise settlement. A waiting turn remains the session's active turn and commits no partial transcript merely because it waits. A waiting process records its wait as an observation of running work. Process events describe the wait; they do not resolve it.

## Rules and guarantees

The promise address derives from the structured scope and wait identity. The first terminal resolution is retained. A repeated resolve returns the recorded terminal result even when its proposed payload differs. Unknown or revoked addresses remain distinguishable from runtime failures. Inbound resolution uses an ordinary object call; retained terminal state supplies deduplication.

## Alternatives and consequences

Making a suspended turn a process is rejected because a session-owned turn must not acquire process addressability and lifecycle. Requiring authors to start a process before a long tool call is rejected because suspension is an execution concern. Sharing the promise mechanism preserves those separate ownership rules.

The implementation is in [durable wait identities and promises](../../crates/lash-restate/src/durable_wait.rs) and [resolve ingress](../../crates/lash-restate/src/effect_host/ingress.rs). ADR 0012 describes engine journaling and deadline replay.
