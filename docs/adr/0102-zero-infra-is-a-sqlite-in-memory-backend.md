# 0102: Every backend binds one journaled engine to one store set

## Status

Accepted.

## Context

A runtime needs a coherent set of session, process, trigger, attachment and
module-artifact stores, together with the engine that executes its work.
Constructing those ports independently can make a session write state that its
reopen or cleanup path cannot reach. Tests need inexpensive storage with the
same SQL contracts as a file or PostgreSQL deployment.

## Decision

### D1. Every durable host journals

The effect engine owns recording and replay. A backend supplies its engine's
`EffectHost`; SQLite and PostgreSQL supply storage through `StoreSet`.
Restate is the shipping engine, as specified by
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md).
A code cell reconstructs local execution state by re-execution over its nested
journaled effects under [ADR 0103](0103-code-cells-replay-by-re-execution-on-every-host.md).

Evidence: `crates/lash-core-execution/src/backend.rs:39`,
`crates/lash-restate/src/engine.rs:113`,
`crates/lash-restate/src/controller/execution.rs:122`.

### D2. One backend supplies every port

`Backend` contains one private `Arc<dyn EffectEngine>`. Its effect and work
ports come from that engine; its storage ports come from the engine's one
`StoreSet`. Cloning shares the engine. The process registry accessor returns
the registry of the engine's process-work wiring, so callers use the same
registry as the engine rather than an independently assembled handle.

`RuntimeHostConfig::new` requires a backend. Plugin factories that bind to a
backend declare that binding, and core refuses a factory bound to another one.
Storage identity (`StoreBindingId`) and effect authority name different state;
the engine fixes both at construction. Storage identity is not an effect fence.

A SQLite store set supplies its storage ports from one typed location. A
PostgreSQL store set supplies them from one `PostgresStorage` and takes its
attachment-byte store at construction. Module artifacts belong to the store
set that reopens the session. The kernel's contracts name neither a concrete
SQL store nor a concrete engine.

Evidence: `crates/lash-core-execution/src/backend.rs:89`,
`crates/lash-core-execution/src/backend.rs:155`,
`crates/lash-core-execution/src/runtime/host.rs:185`,
`crates/lash/src/plugin_binding.rs:1`,
`crates/lash-sqlite-store/src/backend.rs:95`,
`crates/lash-postgres-store/src/postgres/backend.rs:44`.

### D3. SQLite memory is named SQL storage

`SqliteStoreSet::memory()` creates the durable-core, process-registry and
trigger databases as named `memdb` databases. Each connection reaches its
database through `file:/lash-<uuid>/<db>?vfs=memdb`. The store set pins one
anchor connection per database; reopened handles share those anchors.
The data lives while any owning handle keeps the anchors alive.

`SqliteLocation` supplies both database addresses and the storage identity.
A file location names `sqlite:<canonical durable-core.db path>`; a memory
location names `sqlite-memory:<uuid>`. Path-taking constructors refuse raw
`:memory:` and `file:` strings. Memory storage is explicit, through the typed
constructor, and supplies no effect engine or restart durability.

Named memory databases use SQLite's `memory` journal mode. They have no WAL;
a store operation must release a write transaction before awaiting work on
another connection that needs to read that database.

The live-replay stream buffer is an observation cache. It does not replace any
persistence port or become the authority for a commit.

Evidence: `crates/lash-sqlite-store/src/backend.rs:176`,
`crates/lash-sqlite-store/src/backend.rs:211`,
`crates/lash-sqlite-store/src/location.rs:28`,
`crates/lash-sqlite-store/src/location.rs:47`,
`crates/lash-sqlite-store/src/location.rs:165`,
`crates/lash-sqlite-store/src/conn.rs:547`,
`crates/lash-conformance/src/live_replay_store_tests.rs:1`.

### D4. Explicit construction and features

The facade has no default features. `sqlite` enables SQLite storage, and
`testing` does not implicitly enable it. A durable local application runs a
local Restate server over a store set, under ADR 0104 §4. A test can use the
in-process Restate server double over the same storage contracts.

Storage laws cover SQLite file, SQLite memory and PostgreSQL. Execution hosts
are the in-process Restate server double, live Restate and lash-sim's
in-process effect host. Lash-sim's `SimEngine` executes the production Restate
handlers on the double over SQLite memory storage.

Evidence: `crates/lash/Cargo.toml:61`,
`crates/lash-sqlite-store/tests/conformance.rs:1`,
`crates/lash-sqlite-store/tests/conformance_memory.rs:1`,
`crates/lash-postgres-store/tests/conformance.rs:1`,
`crates/lash-restate-test/src/backend.rs:82`,
`crates/lash-sim/src/backend.rs:35`.

## Rejected alternatives

- Independent per-port defaults can put the session and its artifacts or
  process records in different storage. One backend carries the binding.
- A hand-written memory persistence implementation duplicates the SQL
  contracts. Named SQLite memory databases exercise the SQL implementation.
- File-only test storage requires a filesystem lifetime for tests that need
  only a process lifetime. Named memory storage retains shared connections.
- A shipping in-process test host needs durable journal storage, scheduling
  and recovery. Those are engine responsibilities under ADR 0104.

## Consequences

Embedders select an engine and a store set explicitly. Memory storage shares
SQL semantics and costs local SQL work, but loses its data when its owning
handles disappear. A durable local application also runs the engine server.
Storage and effect identities remain distinct, and every runtime port comes
from the backend's construction.
