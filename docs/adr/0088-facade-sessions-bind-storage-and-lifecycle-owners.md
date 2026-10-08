# Facade sessions bind storage and lifecycle owners

## Context

Per-session storage and lifecycle services must address one backend consistently.
Resolving them independently during cancellation, observation, or resume can
apply control to a different deployment from the one that owns the session.

## Decision

Every opened facade session has one privately constructed, immutable
`BoundSession`. It retains the exact session store, backend catalog, durable
engine, process/queue ports, attachment and process-environment stores, process
engines, close services, deferred-work policy, and resident-session
registration.
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
to a receiving core environment restores the owning backend, durable engine,
attachment store, process-environment store, and work ports. The receiving core
supplies only physical binding: live provider resolution, the implementations
of its one plugin set, and tracing. Behaviour is recorded config. The durable
session config, including generation, the recorded model with its request
defaults, the execution controls of ADR 0030, the core `PromptPlan` and every
installed plugin's namespace (ADR 0126), remains
recorded config under ADR 0074. A core holds no session defaults (ADR 0030):
what its host passes to the sessions it creates never reaches a resumed
session.

### Behaviour is recorded config; live is physical only

A core installs one plugin set, and every session, open, resume, actor
activation and process of that core runs that set. No open, resume or process adds
plugins of its own: a session's behaviour is the plugin config it recorded at
creation, changed only by its owners' typed config commands. An open still
states physical facts — the provider that serves the recorded route and the
tool-source policy that refuses an unavailable source — and neither selects
behaviour.

An engine or tool-call process runs under its starter's captured execution
environment. A session-turn process instead records a new child environment
from the supplied policy and model, tool authority, prompt plan and plugin
creation options. Parentage copies no configuration. The start resolves the
child's complete facts with its installed plugin set before registration;
the worker binds that set and uses the recorded environment, supplying no
replacement policy or plugin configuration. A child that the set cannot
create is a typed `session_config_refused`, never a retried infrastructure
error. A tool-declared session-turn start without the required execution
environment is `ExecutionEnvMissing` at intent admission, before any start
command is recorded. Completed batches refuse together; pending declared
starts settle the call with a non-retryable refusal.

A deployment that needs an incompatible plugin set — another protocol, or
owners whose recorded namespaces the first set does not register — runs it on
a separate deployment: its own store set, with its own cores and nodes
([ADR 0102](0102-zero-infra-is-a-sqlite-in-memory-backend.md) D2). One store
set never mixes plugin sets.

Durable turn input crosses mandatory acceptance. Recorded protocol options
and durable RLM seeds can cross that boundary. Process-local projection
configuration belongs to runtime materialization, rather than serialized turn
input. Exact-session work ports and lifecycle operations retain their binding
instead of rediscovering storage from a receiving core.

### Park and close

The bound turn owns the session head, so park and close never commit a
whole-session snapshot beside the session actor's owner. A session whose runtime holds nothing
unpersisted parks and closes without writing. A dirty one (plugin state, graph
nodes, or pending usage no commit carried yet) adopts the durable head and then
flushes. Adopting a head that moved replaces the resident state with that
head's, as a cold rebuild from it would: plugin writes and graph nodes the
runtime accepted on the older head are dropped, and pending usage, held apart
from the head, rides the flush. While a run is bound, a follow-on is owed, or a
session command is open, the store refuses that flush in its own transaction as
`StoreError::SessionHeadOwned`, naming the owner. The refusal is typed and
recoverable: `LashSession::park` and `close` answer `SessionParkRefused` and the
runtime's `park` answers `ParkRefused`. Each names the busy owner and hands the
session back with its runtime, resident state, and pending usage intact. The
host keeps using the session, or parks it again once the owner's boundary
passes; that park adopts whatever the boundary committed and lands. A park
refused because another handle still shares the runtime (`SessionStillInUse`)
leaves that handle in place.

### Administration

Catalog administration is separate from an opened session. Deletion uses the
backend-issued close and delete services derived from the owning backend.
`SessionDeleteContext::from_execution` captures that administration and derives
the exact session-delete scope from the requested id. Callers cannot supply a
scope for one session alongside another session's administration services.
Its deferred deletion work can complete after permanent session tombstoning:
the closing session actor closes its scopes in its own transactions, and
physical deletion is the `SessionDelete` deferred work of ADR 0109 §4.

Hosts installing third-party stores own the truthful physical pairing of the
catalog and lifecycle services. Rust ownership can preserve an issued pairing;
it cannot verify an external store's routing. Lash adds no authentication
or security-policy decision to that composition.

## Alternatives considered

An optional store after open cannot satisfy unconditional durable acceptance.
Independent lookups during lifecycle operations can switch backend ownership.
Rebinding a parked session to the receiving core's storage loses its exact
continuation owner. One captured binding prevents those substitutions.

## Consequences

Every facade execution has a real catalog-backed store. Resume retains lifecycle
ownership while accepting current physical wiring and never a different
behaviour. Session relationships use one
ordinary session model, and deletion retries use durable deferred work. The host
owns external deployment composition.

## Code references

- `crates/lash/src/session.rs:152-169,247-275,518-541` separates create and existing-session resolution.
- `crates/lash/src/session_binding.rs:6-63,150-185` captures owner services and applies them on resume.
- `crates/lash/src/core.rs` (`build_plugin_host`, `durable_process_worker_config`) builds the core's one plugin set for opens, activations and processes.
- `crates/lash-core/src/runtime/process_runtime.rs` (`ProcessRuntimeContext::for_admitted`) builds every process runtime from its captured environment; `crates/lash-core/src/runtime/session_manager/session_init.rs` (`resolve_child_facts`, `admit_session_turn_child`) resolves and admits a session-turn child's facts.
- `crates/lash-core/src/runtime/lifecycle.rs` (`park`, `flush_for_park`) and `crates/lash-core/src/runtime/environment.rs` (`ParkRefused`) make a busy park recoverable.
- `crates/lash-core/src/runtime/session_administration.rs:104-153` issues the paired deletion context.
- `crates/lash/src/tests/core_session_builder/session_lifecycle/session_binding.rs` pins lifecycle-owner behavior.
