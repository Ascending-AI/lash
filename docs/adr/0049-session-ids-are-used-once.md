# Session ids are used once

## Status

Accepted.

## Context

A session-keyed history node, turn address or revocation must identify one
lifetime. Lash cannot prove global uniqueness of an arbitrary host string, but
it can enforce non-reuse within the store that admits it.

## Decision

A host-facing session id is host-provided, nonempty, NUL-free UTF-8 and opaque
to Lash. A transport may impose narrower syntax. Once a store materializes
session metadata for an id, that id identifies one session lifetime.
An accepted close first makes it refuse new work. Physical deletion follows
its owed cleanup and writes permanent deletion evidence. A `Closing` result
has not yet deleted storage; `LashCore::await_session_deletion` observes the
permanent tombstone and returns typed stalls that need operator re-arm.
Creating or forking to that
id fails with `StoreError::SessionDeleted`. Retention and vacuum cannot remove
this evidence. Deleting an id that never materializes is a no-op.

Catalog admission of the same live id is idempotent when its binding agrees:
`SessionAdmission::{Created, Rebound}` distinguishes creation from rebind.
Facade `create` is stricter and refuses an existing id with
`SessionAlreadyExists`; `open` is the explicit non-creating operation.
`fork_at` takes a new host id and checks the same permanent deletion fence.

Binding includes session relation and ownership. Admission materializes and
checks it atomically. Ordinary history-node ids use session id, operation id
and ordinal; frame ids use session id and frame key. Turn addresses and
session-scoped durable identities use that same session id, with no separate
session-lifetime discriminator. Facade sessions bind explicit storage and
lifecycle owners (ADR 0088).

### Scope fences are a permanent-row class with one release rule

Process and runtime-operation scopes have no session id, so deleting a session
does not revoke their waits. Scope-exact retirement supplies their own
revocation boundary: it revokes every wait row of the scope and records the
revocation, so later wait admission and resolution under the revoked scope
refuse ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §6).

Retirement requires named reachability proof. An owner-terminal retirement
can revoke the scope; a process's terminal transaction revokes its own pending
waits (ADR 0132 §11). `WhenQuiescent` refuses while started executions without
an outcome, unsettled group children or unresolved wait rows remain; its
refusal leaves the scope unfenced. A returning operation receipt alone does
not prove quiescence. Retention and lifecycle cleanup use the host's explicit
levers.

Process ids are minted and single-use (ADR 0107), and runtime-operation ids
are used once. Session revocation cannot be lifted. Ordinary registration
never reuses a pruned process id.

## Consequences

Deleting a session is final for its id. Reset creates a new id rather than
reopening a deleted lifetime. Tombstones, retained graph and wait revocation
must keep a consistent lifecycle; independently wiping identity evidence can
violate non-reuse. Adding another incarnation field is rejected because every
session-keyed identity can rely on the same permanent admission fence.

Fork creation and observer publication cross transaction domains. A fork
retains pending observer intent until publication settles and the intent clear
commits. Opening reconciles an interrupted publication. `Unavailable` retains
the selection for retry; missing and pruned selections have typed terminal
outcomes. Reasserting already-published edges is permitted until the intent
clear. The single-use fork id cannot alias a later lifetime.

## Implementation

- [ID validation and closure pins](../../crates/lash-core-store/src/store/mod.rs).
- [SQLite admission](../../crates/lash-sqlite-store/src/catalog.rs), [PostgreSQL admission](../../crates/lash-postgres-store/src/postgres/session_factory/store.rs) and [facade create/open](../../crates/lash/src/session.rs).
- [Observer intent reconciliation](../../crates/lash-core-execution/src/runtime/process/observer_intent.rs).
