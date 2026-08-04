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
`schema-shape.txt`, and hard-fails on mismatch by default.

The check describes **one installation**. Every object is read from a single
namespace — the one where `lash_schema_versions` resolves through the connection's
`search_path`, which is by construction the installation lash's own unqualified
statements use — and every expected object must be present there. Resolving each
table independently is not a smaller version of this: with
`search_path = front, back` and one table missing from `front`, per-table
resolution accepts the union of two halves, each individually well-shaped, while
runtime writes split across both. A lash-named relation that `search_path`
resolves *outside* the anchored namespace is reported rather than used, because
that is where lash would actually write.

The scope is narrow and chosen for cross-version stability: per lash-owned table,
columns as (name, type, nullability, value source), every uniqueness guarantee read
from `pg_index.indisunique` including normalized partial predicates and
`NULLS NOT DISTINCT`, and foreign keys with their on-delete action. `CHECK`
constraints, non-unique indexes, triggers, row-level security, and default
expression text are out of scope. The predicate of a partial unique index is the
only free-text element in the whole comparison.

Objects are matched by **what they enforce**, never by how they were written.
Columns are matched by name and never by ordinal position. Guards and keys are
matched by their column *set* and kind, never by constraint name and never by column
order: `UNIQUE (a, b)` and `UNIQUE (b, a)` reject exactly the same rows, so a host
that rebuilt one as the other has not drifted. Key order does change which index
prefixes can be scanned, which is an access-path property — the same class this check
declines to verify when it leaves non-unique indexes out entirely. What a same-set
pair *is* compared on is what changes the rows it covers: the partial predicate and
null-distinctness for a guard, the delete action for a foreign key. Splitting
matching from comparison this way is what lets a same-set guard with a different
predicate report as one precise mismatch rather than an unrelated
missing-plus-unexpected pair. For a composite foreign key the matched identity is
the *set of column pairings*, so `(a, b) -> (x, y)` stays distinct from
`(a, b) -> (y, x)` while `(b, a) -> (y, x)` is recognized as the same constraint.

A column's **value source** is classified rather than reduced to a single
has-an-auto-generated-value bit, because what lash's inserts depend on is two
properties, not one: whether the column supplies its own value when an insert
omits it, and whether it *accepts* one when an insert names it.
`GENERATED ALWAYS AS IDENTITY` and generated columns supply a value and reject
every explicit one, and lash names `enqueue_seq` in its inserts (it needs the value
to derive `batch_id`) and writes `head_revision` on every session-head commit. A
single bit accepted both drifts and then failed every enqueue and every commit. The
comparison is by write capability, so the legitimate
`BIGSERIAL` → `GENERATED BY DEFAULT AS IDENTITY` modernization still passes while
`GENERATED ALWAYS` does not.

Four properties of that scope are load-bearing rather than incidental. Reading
uniqueness from `pg_index` rather than `pg_constraint` is what makes the
motivating case detectable at all: a partial unique index has no `pg_constraint`
row, so a constraints-only check would miss exactly the guard the decision exists
to protect. Filtering `pg_constraint` to `contype = 'f'` is what keeps
PostgreSQL 18 from rejecting every host at once, because 18 materializes NOT NULL
as `contype = 'n'` rows and an unfiltered enumeration would gain dozens of them
per table on upgrade. Order- and name-insensitivity is what lets a schema
composed by `ALTER` — the shape a migration tool produces when it edits an
existing baseline — pass, which is the workflow the whole decision serves. And
`NULLS NOT DISTINCT` is in scope despite `pg_index.indnullsnotdistinct` not
existing before PostgreSQL 15, because two of lash's guards cover a nullable
`source_key` that it writes `NULL` into for every keyless batch: under
`NULLS NOT DISTINCT` only one such row per session survives. Naming the column is
a parse error on 14, so it is read through `to_jsonb`, where an absent key yields
NULL and normalizes to the pre-15 semantics — the version-conditional read costs
one SQL expression and keeps the artifact identical across the matrix.

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

`SchemaCheck` governs the catalog comparison and nothing else. The component
version stamp is the reject-and-recreate boundary and is **unconditionally fatal**
in every mode: if the valve could downgrade it, a host that adopted the valve for a
structural false positive would later open silently against a pre-cutover database
— process events with no completion-authority payload, manifest rows naming a blob
layout that cannot be read — which is the corruption the boundary exists to
prevent. The signing-secret row is likewise a data precondition outside the valve:
without it there is no key to authenticate durable promises with, so there is
nothing for open to return.

`PostgresStorage::verify_schema_for(&pool)` exposes the same check as a structured
report without failing and without opening, so a host gates its own migration CI on
it and a production open becomes the backstop that never fires rather than the place
drift is discovered. It deliberately takes a pool rather than a constructed store:
opening is strictly harder than verifying — open additionally demands a matching
version stamp and a usable signing secret, either of which can be exactly what a
migration produced wrongly — so a check reachable only through a successful open
could not describe the databases it exists to describe.

lash takes one published advisory key (`PostgresStorage::schema_advisory_lock_key`)
for everything it does to the schema: exclusively while provisioning and opening,
and in shared mode inside the single transaction `verify_schema_for` runs, so
concurrent verifications do not conflict with each other while any exclusive holder
excludes them all. Taking it in the verification path is not decoration — the same
docs recommend that entry point for migration CI, and a key that covered opens but
not verification would be a protocol with a hole exactly where hosts were told to
use it.

What the key cannot do by itself is coordinate a participant that ignores it: a host
migration committing between two catalog reads can make an open succeed against a
schema that has already drifted, or refuse one that is mid-migration and fine.
Holding relation locks strong enough to close that would mean blocking a host's own
DDL from inside lash's open path, which is a worse trade. The supported protocol is
therefore to take this key around migrations — which is also what a migration CI job
should wrap around its migrate-then-verify sequence — or to guarantee no migration
runs concurrently with an open.

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
  because lash owns those tables by contract and each can reject writes lash
  considers valid.

  This is a **deliberate deviation, ratified rather than inherited**: the original
  ruling named only extra columns as a mismatch, and rejecting host-added unique
  indexes and foreign keys is stricter than that text. It is accepted on the
  merits — a host-added unique guard on a lash table can reject a write lash
  considers valid, and the false-positive population is close to empty — and the
  asymmetry it creates is accepted with it. Host-added triggers, `CHECK`
  constraints, and row-level security can break lash's writes just as thoroughly
  (RLS with no policy makes lash read an empty database silently) and are *not*
  read, so they remain the host's own risk. The line is drawn at the object classes
  the fingerprint already reads for other reasons; widening it to triggers, checks,
  and RLS would be a separate decision with its own false-positive surface, and
  drawing it there is a choice rather than an oversight.

- A pre-version-9 database's leftover `lash_process_change_seq` sequence is never
  cleaned up now that the artifact is creation-only. Such a database is rejected at
  open by the version gate regardless, so the sequence is unreachable garbage an
  operator deletes manually — the same disposition as the pre-cutover blob prefixes
  earlier version bumps left behind.

- The check cannot be made safe against a host migrating concurrently with an
  open. That is documented as a protocol rather than engineered around, and the
  advisory key is published so a host that needs coordination has one. Everything
  lash itself does to the schema takes that key.

- The gate emits its full decision basis — stamped and expected version, both policy
  knobs, and finding counts per class — on every outcome it can reach, denial and
  admission alike, per the decision-evidence standard in
  `docs/agents/way-of-working.md`. A refused open is diagnosable from a trace
  without reproducing it.

- A report's remedy is chosen per finding class. Version findings get the
  reject-and-recreate instruction and deliberately do *not* mention
  `SchemaCheck::WarnOnly`, because that valve cannot open past the version
  boundary; recommending it there would be advice a host cannot follow.
- The await-event signing secret is a data precondition rather than a shape, so
  `SchemaCheck::WarnOnly` does not relax it: without the row there is no secret
  to authenticate promises with, and the store cannot construct itself.
