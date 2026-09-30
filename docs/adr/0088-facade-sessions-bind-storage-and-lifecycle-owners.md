# Facade sessions bind storage and lifecycle owners

## Context

Per-session storage and lifecycle services must address one backend consistently.
Resolving them independently during cancellation, observation, or resume can
apply control to a different deployment from the one that owns the session.

## Decision

Every opened facade session has one privately constructed, immutable
`BoundSession`. It retains the exact session store, backend catalog, effect
host, process/queue ports, attachment and process-environment stores, process
engines, close services, relay policy, and resident-session registration.
Per-session operations derive their capabilities from that binding.

The store comes from the core's catalog. `SessionBuilder::create(SessionCreation)`
is the only facade creation verb; it writes the catalog row and initial config
head atomically. `open` resolves an existing id without creating it. An absent
id returns `UnknownSession`, and a deleted id returns `SessionDeleted` before
execution. There is no explicit builder store or storeless facade mode.

Related sessions use the same admission and binding model under ADR 0089. A
parent relation does not substitute the parent's exact session store or create
a second facade session model.

### Resume and control ownership

A parked session retains its owner binding across resume. Applying that binding
to a receiving core environment restores the owning backend, effect host,
attachment store, process-environment store, and work ports. The receiving core
supplies live provider resolution, plugin factories, tracing, and live policy.
The durable session config, including session prompt and generation, remains
recorded config under ADR 0074; the live core prompt is separate.

Durable turn input crosses mandatory acceptance. Replayable protocol options
and durable RLM seeds can cross that boundary. Process-local projection
configuration belongs to runtime materialization, rather than serialized turn
input. Exact-session work ports and lifecycle operations retain their binding
instead of rediscovering storage from a receiving core.

### Park and close

The bound turn owns the session head, so park and close never commit a
whole-session snapshot beside the drive. A session whose runtime holds nothing
unpersisted parks and closes without writing. A dirty one (plugin state, graph
nodes, or pending usage no commit carried yet) adopts the durable head and then
flushes. While a root is bound, a follow-on is owed, or a session command is
open, the store refuses that flush in its own transaction as
`StoreError::SessionHeadOwned`, naming the owner. The refusal is typed and
recoverable: `LashSession::park` and `close` answer `SessionParkRefused` and
the runtime's `park` answers `ParkRefused`. Each names the busy owner and hands
the session back with its runtime, resident state, and pending usage intact.
The host keeps using the session, or parks it again once the owner's boundary
passes. A park refused because another handle still shares the runtime
(`SessionStillInUse`) leaves that handle in place.

### Administration

Catalog administration is separate from an opened session. Deletion uses the
backend-issued close and delete services derived from the owning backend.
`SessionDeleteContext::from_execution` captures that administration and derives
the exact session-delete scope from the requested id. Callers cannot supply a
scope for one session alongside another session's administration services.
Its obligations can complete after permanent session tombstoning. Engine-side
scope closure and storage-side deletion retain their separate durable duties
under ADR 0109.

Hosts installing third-party backends own the truthful physical pairing of the
catalog, lifecycle services, and engine context. Rust ownership can preserve an
issued pairing; it cannot verify external SDK routing. Lash adds no authentication
or security-policy decision to that composition.

## Alternatives considered

An optional store after open cannot satisfy unconditional durable acceptance.
Independent lookups during lifecycle operations can switch backend ownership.
Rebinding a parked session to the receiving core's storage loses its exact
continuation owner. One captured binding prevents those substitutions.

## Consequences

Every facade execution has a real catalog-backed store. Resume retains lifecycle
ownership while accepting current live wiring. Session relationships use one
ordinary session model, and deletion retries use durable obligations. The host
owns external deployment composition.

## Code references

- `crates/lash/src/session.rs:152-169,247-275,518-541` separates create and existing-session resolution.
- `crates/lash/src/session_binding.rs:6-63,150-185` captures owner services and applies them on resume.
- `crates/lash-core/src/runtime/lifecycle.rs` (`park`, `flush_for_park`) and `crates/lash-core/src/runtime/environment.rs` (`ParkRefused`) make a busy park recoverable.
- `crates/lash-core/src/runtime/session_administration.rs:104-153` issues the paired deletion context.
- `crates/lash/src/tests/core_session_builder/session_lifecycle/session_binding.rs` pins lifecycle-owner behavior.
