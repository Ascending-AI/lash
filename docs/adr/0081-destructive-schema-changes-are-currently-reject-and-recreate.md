# Destructive schema changes are currently reject-and-recreate

## Status

Accepted.

## Context

The SQL stores already expose the seam needed for schema evolution: a component
schema version and at-open migration machinery. SQLite has treated destructive
changes as reject-and-recreate boundaries, while PostgreSQL has sometimes added
an explicit migration arm. That difference left the current release phase
without one predictable rule for changes that can reject rows an older schema
accepted, including new `CHECK` constraints over durable vocabularies.

The Postgres structural fingerprint deliberately excludes `CHECK` constraints
under ADR 0052. Reopening that fingerprint would also reopen its cross-version
catalog-normalization scope. ADR 0067 instead establishes the relevant data
rule: invariants that protect durable rows from writers bypassing the driver
live in the DDL. Driver parsing remains a read-side corruption detector; it is
not a substitute for a database constraint on writes.

## Decision

For the current phase, a destructive PostgreSQL schema change ships as a plain
`SCHEMA_VERSION` bump with no migration arm. A database stamped with the
pre-cutover component version is rejected at open, matching SQLite's existing
destructive-change posture. Operators recreate the affected trust domain from
the new published schema rather than asking Lash to reinterpret or repair rows
accepted by the old schema.

This is a current-phase policy, not a permanent ban on migrations. At present,
`apply_schema_migration` is unreachable: no rows in `SCHEMA_MIGRATIONS` target
the current `SCHEMA_VERSION`. Behavioural coverage of that seam returns with
the next creation-only bump, when a migration row targets the new current
version. Until then, this decision says only that a destructive change does not
acquire such an arm in the current phase.

Vocabulary and kind constraints remain ordinary DDL `CHECK` constraints under
ADR 0067. The lash-sim schema-congruence gate owns a declared expected-
constraints registry for each SQL dialect. Every registry entry names its table,
constraint, and expression, and the gate fails when the published DDL omits it.
The registry is independent of ADR 0052's generated Postgres schema fingerprint;
that fingerprint's scope is unchanged.

Amended 2026-09-11 (FIG-2837): the shared registry is now runtime-reachable as
well as CI-reachable. PostgreSQL exposes explicit read-only inspection through
`PostgresStorage::inspect_required_constraints_for` and
`inspect_required_constraints_on`; SQLite exposes
`inspect_required_constraints_at(path, SqliteDatabase)`. A report covers only
the registered named checks in the inspected snapshot and is not a schema-
version, openability, complete-integrity, or row-validation claim. Unsupported
comparison grammar is a typed inconclusive error. These calls remain outside
ordinary startup and never apply DDL or repair data.

## The store records which release wrote it

A reject-and-recreate boundary is only actionable if the operator can tell which
release is on the other side of it, and component integers alone cannot say. Both
SQL backends therefore carry a durable release stamp — `release_stamp` in the
SQLite durable core, `lash_release_stamp` in PostgreSQL — holding the writing
release's crate version string, the schema versions it required, and when that
release first wrote the store. It is written on the first open of an unstamped
store and advanced only when a strictly newer release opens it; a reopen under
the same release leaves the row alone, and an older build never downgrades it.
Store preflight surfaces the stamp on its schema report as a typed three-state
answer — stamped, unstamped, or unreadable — so a store no stamping build has
written reports an explicit absence rather than an empty release, and a stamp
this build cannot read stays undecided rather than collapsing into that absence.
The same fact rides the open-time refusal: when the stamp is still readable, the
message names the writing release after its existing sentences, so a host that
upgrades into a refusal is told which release reopens the store instead of being
left to work backwards from two integers.

## Consequences

- A destructive cutover advances the Postgres component version and every
  affected SQLite database component version without adding a migration arm.
- Pre-cutover databases fail during open before runtime readers or writers can
  operate on rows whose vocabulary is no longer valid.
- Creation-only migrations and their machinery stay in place as the supported
  evolutionary seam; they are not generalized into destructive migrations by
  this decision.
- New durable `CHECK` constraints must be added to the appropriate dialect
  registry as well as both DDL artifacts. The registry supplements rather than
  expands the ADR 0052 fingerprint.
- The pending-turn-input claim id and claim token are an all-or-none pair in
  both SQL schemas. PostgreSQL component 86 and SQLite durable-core version 57
  are reject-and-recreate boundaries for that new guard; neither backend adds a
  migration arm.
