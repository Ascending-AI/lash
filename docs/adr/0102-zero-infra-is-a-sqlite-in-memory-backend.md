# 0102: Every backend binds one durable engine to one store set

## Status

Accepted.

## Context

A runtime needs a coherent set of session, process, attachment and
module-artifact stores, together with the engine that executes its work.
Constructing those ports independently can make a session write state that its
reopen or cleanup path cannot reach. Tests need inexpensive storage with the
same SQL contracts as a file or PostgreSQL deployment.

## Decision

### D1. Every durable host runs the one durable engine

Lash's durable engine executes all work and persists its state through the
store set
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §1).
SQLite and PostgreSQL supply storage through `StoreSet`; the engine's SQL lives
in one module per dialect. Nothing re-runs to rebuild state: a code cell
resumes from its committed VM snapshot under ADR 0132 §8.

Evidence: `crates/lash-core-execution/src/backend.rs:39`.

### D2. One backend supplies every port

`Backend` builds the durable engine directly over its one `StoreSet`. Its work
ports and storage ports come from that store set. Cloning shares the engine.
The process registry accessor returns the registry of the engine's
process-work wiring, so callers use the same registry as the engine rather
than an independently assembled handle.

`RuntimeHostConfig::new` requires a backend. Plugin factories that bind to a
backend declare that binding, and core refuses a factory bound to another one.
Storage identity (`StoreBindingId`) names the store set; the actor epoch is the
execution fence (ADR 0132 §3). Storage identity is not an execution fence.

A SQLite store set supplies its storage ports from one typed location. A
PostgreSQL store set supplies them from one `PostgresStorage` and takes its
attachment-byte store at construction. Module artifacts belong to the store
set that reopens the session. The kernel's contracts name no concrete SQL
store.

Evidence: `crates/lash-core-execution/src/backend.rs:89`,
`crates/lash-core-execution/src/runtime/host.rs:185`,
`crates/lash/src/plugin_binding.rs:1`,
`crates/lash-sqlite-store/src/backend.rs:95`,
`crates/lash-postgres-store/src/postgres/backend.rs:44`.

### D3. SQLite memory is named SQL storage

A SQLite store set is one database (ADR 0132 §12). `SqliteStoreSet::memory()`
creates it as a named `memdb` database. Each connection reaches it through
`file:/lash-<uuid>/<db>?vfs=memdb`. The store set pins one anchor connection;
reopened handles share it. The data lives while any owning handle keeps the
anchor alive.

`SqliteLocation` supplies the database address and the storage identity. A
file location names `sqlite:<canonical database path>`; a memory location names
`sqlite-memory:<uuid>`. Path-taking constructors refuse raw `:memory:` and
`file:` strings. Memory storage is explicit, through the typed constructor, and
supplies no restart durability.

Named memory databases use SQLite's `memory` journal mode. They have no WAL;
a store operation must release a write transaction before awaiting work on
another connection that needs to read that database.

The live-replay stream buffer is an observation cache. It does not replace any
persistence port or become the authority for a commit.

Evidence: `crates/lash-sqlite-store/src/backend.rs:176`,
`crates/lash-sqlite-store/src/location.rs:28`,
`crates/lash-sqlite-store/src/conn.rs:547`,
`crates/lash-conformance/src/live_replay_store_tests.rs:1`.

### D4. Explicit construction and features

The facade has no default features. `sqlite` enables SQLite storage, and
`testing` does not implicitly enable it. A durable local application is one
SQLite database file with no server process (ADR 0132 §1). A test uses the
same runtime over SQLite memory.

Storage laws cover SQLite file, SQLite memory and PostgreSQL. Laws run the
production runtime over a fault-injecting store with labelled commits, a
virtual clock and `SimNodes` (ADR 0132 §14). Lash-sim runs that runtime over
SQLite memory storage.

Evidence: `crates/lash/Cargo.toml:61`,
`crates/lash-sqlite-store/tests/conformance.rs:1`,
`crates/lash-sqlite-store/tests/conformance_memory.rs:1`,
`crates/lash-postgres-store/tests/conformance.rs:1`,
`crates/lash-sim/src/backend.rs:35`.

## Rejected alternatives

- Independent per-port defaults can put the session and its artifacts or
  process records in different storage. One backend carries the binding.
- A hand-written memory persistence implementation duplicates the SQL
  contracts. Named SQLite memory databases exercise the SQL implementation.
- File-only test storage requires a filesystem lifetime for tests that need
  only a process lifetime. Named memory storage retains shared connections.
- A pluggable engine seam with a second engine beside the store adds a second
  recovery protocol; the store set is the only durable substrate.

## Consequences

Embedders select a store set explicitly. Memory storage shares SQL semantics
and costs local SQL work, but loses its data when its owning handles
disappear. A durable local application needs no server. Every runtime port
comes from the backend's construction.

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
