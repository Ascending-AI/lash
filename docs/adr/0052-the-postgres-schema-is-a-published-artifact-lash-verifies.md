# The Postgres schema is a published artifact lash verifies at open

## Status

Accepted.

## Context

Hosts can provision Lash's PostgreSQL tables through their own migration tools.
A component stamp describes compatibility but cannot prove that a host applied
the required DDL. Missing unique guards or foreign-key delete actions can break
durability without producing a query error. Open therefore checks the live
installation as well as its compatibility stamp.

## Decision

`crates/lash-postgres-store/schema.sql` is the published creation artifact.
`PostgresStorage::schema_ddl()` returns its bytes verbatim. The artifact is
idempotent, creation-only and schema-unqualified, and includes seed rows. A host
vendors these bytes rather than transcribing them.

`teardown.sql`, returned by `teardown_ddl()`, drops exactly the objects declared
by this build's creation artifact with `DROP TABLE IF EXISTS ... CASCADE`.
Artifact consistency tests derive teardown from that object list. Teardown
covers this build's objects; recreating an incompatible installation can
require recreating the Lash-owned schema or database.

Every production open verifies an already-provisioned installation. Open runs
no DDL. `PostgresStorage::migrate` is the separate expand, backfill and contract
operation, governed by ADR 0106 and ADR 0115. The open transaction records the
release stamp and reads the fleet-format row, so verification-only about the
schema does not mean the whole open is read-only.

### One installation

The verifier anchors the installation to the namespace where
`lash_schema_versions` resolves through `search_path`. All expected tables must
exist there. A Lash relation that resolves elsewhere produces a finding;
combining two partial installations is not a valid installation.

Catalog reads join `pg_class` and `pg_namespace` against one captured search
path and then use object IDs. Independent `to_regclass` lookups could disagree
with the transaction's catalog snapshot.

### Structural scope

The normal expected-versus-found comparison reads:

- column name, type, nullability and value source;
- primary keys, unique constraints and unique indexes, including partial
  predicates and null-distinctness; and
- foreign keys, including their column pairings and delete actions.

Columns match by name. Unique guards match by column set, not name or order.
Composite foreign keys match by the set of source/target column pairs, so
reordering a pairing preserves it while exchanging its targets does not.
Value source distinguishes supplying a default from accepting an explicit
value. A by-default identity can satisfy both; an always-generated value cannot
satisfy an insert that supplies the column.

Uniqueness comes from `pg_index.indisunique`, which includes partial unique
indexes without a constraint row. The normal foreign-key enumeration filters
to `contype = 'f'`. Null-distinctness uses a `to_jsonb` lookup so an absent
catalog field has the pre-15 meaning. The generated `schema-shape.txt` is the
expectation, and failures render per-object differences.

The normal comparison excludes non-unique indexes, `CHECK` constraints,
triggers, row-level security, object names, column positions and default
expression text. Extra tables are outside that comparison. Extra columns,
unique guards and foreign keys on Lash tables produce findings.

An `Expanded` compatibility admission has a stricter, write-safety check.
Additional nullable or defaulted columns are accepted; other shape findings
are refused. Added `CHECK` or exclusion constraints and non-internal triggers
on existing Lash tables are also refused. This check applies independently of
`SchemaCheck` and follows the coexistence contract in ADR 0115. The
synthetic-next tier proves its declared expansion and upgrade path.

### Admission policy

`SchemaCheck::Enforce` is the default and refuses normal structural drift.
`WarnOnly` logs the same report and permits that drift. A host chooses the mode
in API configuration, so a verifier false positive has an explicit escape hatch.

`WarnOnly` does not relax component compatibility, expanded write-safety checks,
catalog identity or the fleet's writable format range. The compatibility
registry governs version and reader-floor admission. A missing catalog-identity
seed prevents constructing the store. Other seed findings remain part of the
structural report. Admission evidence is recorded after these preconditions
succeed; refusals carry their decision basis.

### Verification and migration coordination

`verify_schema_for(&pool)` returns a structured report without opening a store
or failing merely because it finds drift. It uses a detached connection, takes
the published advisory key in shared session mode, and only then starts a
`REPEATABLE READ` transaction. Acquiring the lock before the transaction prevents
a queued verifier from taking its snapshot before a participating migration
finishes. Closing the detached connection releases the session lock even if
the future is cancelled.

Open and migration hold the same key exclusively. A host migrating tables takes
that key around its whole migrate-then-verify sequence. While holding it, the
host calls `verify_schema_on(&mut connection)`, which takes no lock and starts
no transaction. The caller owns the lock and snapshot discipline.

The key coordinates participants only. A non-participating migration can commit
outside the report's snapshot. The verifier does not block arbitrary host DDL
with relation locks.

## Why

A stamp or persisted hash is another assertion a host can copy. Reading the
catalog detects the actual missing guard. A sectioned diff names the repair;
a hash mismatch cannot explain it or express tolerated host additions.

The scope separates row-validity guarantees from access-path choices. Index
order and non-unique indexes affect performance; uniqueness and foreign-key
pairings affect valid writes. The expanded check additionally rejects newly
introduced write restrictions needed for coexistence.

## Consequences

- A malformed installation fails before normal runtime queries use it.
- Worker roles need no DDL privilege. Open still needs the permissions required
  to record its release stamp and operate under the fleet format.
- Schema changes require regenerating the expectation and teardown artifacts.
- Hosts own provisioning and migration coordination. A report describes its
  catalog snapshot rather than an unbounded promise about later host DDL.
- Format compatibility follows the registry and ADR 0115. During the pre-1.0
  freeze, stored-shape edits do not introduce version bumps or upcasters.

## Code evidence

- [Published artifacts and open gate](../../crates/lash-postgres-store/src/postgres/schema.rs#L3).
- [Migration API](../../crates/lash-postgres-store/src/lib.rs#L866).
- [Comparison and expanded admission](../../crates/lash-postgres-store/src/postgres/schema_shape.rs#L83).
- [Catalog introspection](../../crates/lash-postgres-store/src/postgres/schema_shape/introspect.rs#L1).
- [Lock and snapshot discipline](../../crates/lash-postgres-store/src/postgres/schema.rs#L249).
- [Schema and artifact laws](../../crates/lash-postgres-store/src/postgres/schema_tests.rs).
