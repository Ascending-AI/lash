# Operational policy stays with the host and Lash exposes levers

## Status

accepted

## Decision

The embedding host owns intake, drain order, deadlines, failover and treatment of stragglers. Lash exposes operations over its own state and resources without orchestrating those policies:

- `LashSession::park(self)` flushes and returns a resumable `ParkedSession`; `LashCore::resume` reconstructs it. `close(self)` flushes and discards the handle. Both require sole runtime ownership and return `SessionStillInUse` while other live handles remain.
- `Provider::close`, factory release through `LashCore::shutdown`, and `TraceSink::flush` expose resource release. Core shutdown stops its node, releasing the node's actors with an epoch bump ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §3), then visits the protocol and common factories, continuing after errors and returning the first error. It does not choose a turn-drain policy.
- Durable ingress and deferred work remain inspectable and cancellable under ADRs 0101 and 0109. Execution belongs to the session actor's owner under its epoch fence. `revoke_durable_waits` cancels current session waits without deleting the session.
- Trigger registrations are durable truth. Hosts wanting workflow reconciliation compare registered state and explicitly update, delete or prune it. Compilation does not infer subscription removal.

Wait revocation distinguishes `cancel_all`, which cancels the current set of wait rows and permits later registration, from `revoke_all`, which retains revocation and rejects future waits under that owner scope (ADR 0132 §6). A host resolves a wait by its completion key; session-bearing scopes also revoke by session.

## Why and alternatives

A shutdown orchestrator cannot know the host's grace budget, traffic rules or deployment model. It is rejected. Resource-release hooks are accepted because they expose owned operations without choosing ordering policy. A provider hook alone is insufficient because sessions, factories, traces, waits and deferred work have separate owners.

## Consequences

Hosts compose drain and failover from explicit calls. Signal handling, readiness endpoints and deployment deadlines stay outside Lash. Lash's durable engine runs over the lash store under ADR 0132. Hosts own auth and security policy. Trigger source declarations do not replace registered subscription state, and Lash does not reset a store automatically.

[Session release](../../crates/lash/src/session.rs), [core shutdown](../../crates/lash/src/core.rs), and [durable ingress](../../crates/lash-core/src/runtime/durable_queue.rs) implement these levers; wait revocation is the wait rows' scope revocation under ADR 0132 §6.
