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

This is a current-phase policy, not a permanent ban on migrations. Future phases
are expected to add migration support where the product requires it. The schema
version and at-open migration machinery remain the intentional seam for that
work. Existing and future creation-only at-open migrations remain valid for
creation-only changes; this decision says only that a destructive change does
not acquire such an arm in the current phase.

Vocabulary and kind constraints remain ordinary DDL `CHECK` constraints under
ADR 0067. The lash-sim schema-congruence gate owns a declared expected-
constraints registry for each SQL dialect. Every registry entry names its table,
constraint, and expression, and the gate fails when the published DDL omits it.
The registry is independent of ADR 0052's generated Postgres schema fingerprint;
that fingerprint's scope is unchanged.

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
