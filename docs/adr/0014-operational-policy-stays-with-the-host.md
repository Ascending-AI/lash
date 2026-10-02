# Operational policy stays with the host and Lash exposes levers

## Status

accepted

## Decision

The embedding host owns intake, drain order, deadlines, failover and treatment of stragglers. Lash exposes operations over its own state and resources without orchestrating those policies:

- `LashSession::park(self)` flushes and returns a resumable `ParkedSession`; `LashCore::resume` reconstructs it. `close(self)` flushes and discards the handle. Both require sole runtime ownership and return `SessionStillInUse` while other live handles remain.
- `Provider::close`, factory release through `LashCore::shutdown`, and `TraceSink::flush` expose resource release. Core shutdown resigns recovery leadership, then visits the protocol and common factories, continuing after errors and returning the first error. It does not choose a turn-drain policy.
- Durable ingress and its delivery obligations remain inspectable and cancellable under ADRs 0101 and 0109. Execution belongs to the sealed shift fence. `revoke_durable_waits` cancels current session waits without deleting the session.
- Trigger registrations are durable truth. Hosts wanting workflow reconciliation compare registered state and explicitly update, delete or prune it. Compilation does not infer subscription removal.

Restate's wait index distinguishes `cancel_all`, which cancels the current set and permits later registration, from `revoke_all`, which retains revocation and rejects future waits. Exact workflow addresses resolve promises; session-bearing scopes also participate in the session index.

## Why and alternatives

A shutdown orchestrator cannot know the host's grace budget, traffic rules or deployment model. It is rejected. Resource-release hooks are accepted because they expose owned operations without choosing ordering policy. A provider hook alone is insufficient because sessions, factories, traces, waits and delivery obligations have separate owners.

## Consequences

Hosts compose drain and failover from explicit calls. Signal handling, readiness endpoints and deployment deadlines stay outside Lash. SQL stores provide storage; the engine provides durable execution under ADR 0104. Hosts own auth and security policy. Trigger source declarations do not replace registered subscription state, and Lash does not reset a store automatically.

[Session release](../../crates/lash/src/session.rs), [core shutdown](../../crates/lash/src/core.rs), [durable ingress](../../crates/lash-core/src/runtime/durable_queue.rs) and [wait indexing](../../crates/lash-restate/src/durable_wait.rs) implement these levers.
