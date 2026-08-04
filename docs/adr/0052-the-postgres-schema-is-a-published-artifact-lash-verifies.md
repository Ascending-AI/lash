# The Postgres schema is a published artifact lash verifies at open

## Status

Accepted.

## Context

`lash-postgres-store` embedded its DDL as a Rust string literal and gated every
open on one integer: the component row in `lash_schema_versions`. That gate is
sound only while lash is the sole writer of the DDL. It is not. A host may
provision the database itself — the downstream host figments transcribes lash's
DDL into its own goose migrations and stamps the version row itself — and goose
has no checksum mechanism at all, so a hand-edited or mis-ported migration is
invisible to it.

The consequence is a gate that verifies an assertion the host authored. A schema
that stamps the right number opens cleanly and then fails at the first query, or
worse does not fail at all: dropping `idx_lash_process_events_key` removes the
exactly-once dedup guard for process events without any statement ever erroring,
and dropping an `ON DELETE CASCADE` leaves orphan rows behind process pruning.
Both are silent losses of a durability property lash advertises.

Two further facts shape the decision. `ensure_schema` ran on every construction
path including `from_pool`, so opening lash required `CREATE` on the schema even
when the host had already provisioned it. And no comparable durable-execution or
queue system verifies schema *shape* at startup: Temporal, River, Oban, pg-boss,
graphile-worker, Kong, and Keycloak all compare a version number or a
migration-file checksum. The one structural verifier in the category, pg-boss's
`detectSchemaDrift`, is deliberately opt-in. The systems that do implement this
exact check automatically — Android Room, EF Core, Atlas — all pair a hard-fail
default with a documented escape hatch and a narrow attribute scope, because a
check that reads a live catalog is itself a piece of software that can be wrong.

## Decision

The DDL is a published artifact and the live catalog is the authority.

`crates/lash-postgres-store/schema.sql` is the single source of truth for the
schema. `PostgresStorage::schema_ddl()` returns those bytes verbatim, so the
artifact and the DDL lash executes cannot drift apart. The artifact is
creation-only, idempotent, and unqualified — no `public.`, no `DROP` — so a host
can apply it into any schema with nothing but `CREATE`, and it carries all three
seed rows a working database needs, including a 32-byte await-event signing
secret drawn from the server's strong RNG. Hosts copy the bytes; they never
transcribe them.

Every open ends in a structural check against a generated expectation artifact,
`schema-shape.txt`, and hard-fails on mismatch by default. The scope is narrow
and chosen for cross-version stability: per lash-owned table, columns as
(name, type, nullability, has-auto-generated-value), every uniqueness guarantee
read from `pg_index.indisunique` including normalized partial predicates, and
foreign keys with their on-delete action. Columns are matched by name and never
by ordinal position; guards and keys are matched by column set and kind and never
by constraint name. `CHECK` constraints, non-unique indexes, default expression
text, and `NULLS NOT DISTINCT` are out of scope. The predicate of a partial unique
index is the only free-text element in the whole comparison.

Three properties of that scope are load-bearing rather than incidental. Reading
uniqueness from `pg_index` rather than `pg_constraint` is what makes the
motivating case detectable at all: a partial unique index has no `pg_constraint`
row, so a constraints-only check would miss exactly the guard the decision exists
to protect. Filtering `pg_constraint` to `contype = 'f'` is what keeps
PostgreSQL 18 from rejecting every host at once, because 18 materializes NOT NULL
as `contype = 'n'` rows and an unfiltered enumeration would gain dozens of them
per table on upgrade. And order- and name-insensitivity is what lets a schema
composed by `ALTER` — the shape a migration tool produces when it edits an
existing baseline — pass, which is the workflow the whole decision serves.

The failure is reported as a sectioned expected-versus-found diff naming every
drifted object, never as a hash comparison. A hash cannot express the tolerance
the design requires (host additions outside lash's tables are a subset relation,
and subsets do not hash) and gives the one person who will ever read it nothing
to act on.

Two open options carry the policy:

- `SchemaProvisioning::{LashManaged, HostProvisioned}`. `LashManaged` is the
  default and applies the DDL then verifies, as before. `HostProvisioned` runs no
  DDL whatsoever: it reads the version stamp, verifies the structure, and verifies
  the seed rows, needing no privilege beyond `SELECT` on lash's tables.
- `SchemaCheck::{Enforce, WarnOnly}`. `Enforce` is the default. `WarnOnly` logs
  the same diff and opens anyway, and exists for the one false positive testing
  cannot rule out — lash's own expectation being wrong for a PostgreSQL build it
  has not seen, which would otherwise brick a fleet with no remedy but a release.
  It is an API-level choice a host commits to in reviewed code, deliberately not
  an environment variable an operator can flip during an incident.

`PostgresStorage::verify_schema()` exposes the same check as a structured report
without failing, so a host gates its own migration CI on it and a production open
becomes the backstop that never fires rather than the place drift is discovered.

The expectation artifact is regenerated from a live database rather than
hand-written, and CI runs the Postgres lane on PostgreSQL 14, 16, and 18,
asserting all three produce the byte-identical artifact. Any attribute that
renders differently across the matrix leaves the scope; it is never special-cased
per version.

No fingerprint is persisted in `lash_schema_versions`. A published hash is
exactly as copy-pasteable as an integer, so it would defend against typos rather
than against hosts doing what hosts do, and it would create a second source of
truth that can disagree with the catalog — reintroducing the class of bug this
decision removes. Nothing persisted can be more trustworthy than reading the
catalog.

The promise is scoped to PostgreSQL. `lash-sqlite-store` initialises only an empty
database and lash is the sole writer of its DDL, so there is no foreign hand to
verify against; shape-checking there would be lash verifying itself.

## Consequences

- A mis-ported vendored schema fails at open with the drifted object named
  instead of failing at first query or not failing at all. This is the entire
  value, and the alternative it replaces is a silently broken exactly-once
  guarantee in production.
- A host under restricted grants can now open lash at all. Before this decision
  every construction path required `CREATE`.
- Adding or changing a table means regenerating `schema-shape.txt` against a live
  PostgreSQL. The regeneration is one command and the drift test names it; a
  version bump without it fails a unit test that needs no database.
- lash now has a public supported-PostgreSQL-version contract. A boot gate over
  catalog attributes implies one, and 14 through 18 is what CI asserts.
- Extra tables and extra non-unique indexes anywhere are tolerated. Extra
  columns, unique guards, and foreign keys *on lash's own tables* are reported,
  because lash owns those tables by contract and each can break its writes.
- The await-event signing secret is a data precondition rather than a shape, so
  `SchemaCheck::WarnOnly` does not relax it: without the row there is no secret
  to authenticate promises with, and the store cannot construct itself.
