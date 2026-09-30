# SQL stores refuse unsupported schemas and report the writing release

## Context

A store's schema stamp must tell the binary whether its readers and writers can
operate safely. A refusal also needs an actionable remedy and the identity of
the release that writes the store. Component integers alone do not identify a
release.

## Decision

SQL stores admit schemas through explicit component compatibility contracts.
A schema outside the supported range, or one with no applicable declared
migration, is refused before ordinary runtime use. Driver parsing detects
corrupt values; DDL constraints protect rows from every writer under ADR 0067.
The PostgreSQL structural fingerprint excludes `CHECK` constraints under
ADR 0052, so constraint conformance has independent executable evidence.

The pre-1.0 version freeze applies to shape edits. There is no blanket rule
that each destructive edit adds a component-version bump. Physical migrations,
reader windows, and the clean-slate compatibility floor follow
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).
SQLite's production migration catalog starts empty at that floor; the
synthetic-next tier supplies adjacent creation migrations. Supported migration
and unsupported-generation refusal are distinct outcomes.

For a PostgreSQL generation that cannot migrate, the remedy drains affected
sessions and recreates the whole Lash trust domain. It provisions the Lash
schema with `lash migrate` or this build's published schema artifact and resets
Restate state that refers to those sessions. Current teardown statements alone
cannot promise to remove an incompatible catalog's tables.

## The store records which release wrote it

SQLite durable core has `release_stamp`; PostgreSQL has `lash_release_stamp`.
The row contains the crate release string, required schema-version tuple, and
the time that release first writes the store. A writing open stamps an
unstamped store and advances only to a release the comparison orders strictly
newer. Same-release and older opens leave it unchanged; an unorderable pair
also preserves the row. A read-only PostgreSQL opener does not claim to write
a release stamp.

Preflight reports `Stamped`, `Unstamped`, or `Unreadable`. Missing data is
explicit absence; malformed or unreadable data is an undecided observation.
When the writing release remains readable, schema refusal names it so the
operator can identify the compatible build.

## Alternatives considered

Repairing arbitrary incompatible rows at open cannot establish a total
conversion without a declared source mapping. Refusal exposes the unsupported
boundary instead. Expanding the structural fingerprint to catalog-normalized
`CHECK` expressions adds a separate normalization contract; direct constraint
laws test the write guards without that expansion.

Using only component integers forces operators to infer the writing release.
Collapsing an unreadable release stamp into absence gives a false answer about
who writes the store.

## Consequences

Operators can distinguish a supported migration from a refusal requiring
recreation. Runtime admission cannot bypass physical compatibility. DDL owns
write constraints, and release-stamp observations remain separate from schema
compatibility decisions.

## Code references

- `crates/lash-postgres-store/src/postgres/schema.rs:434-460` defines the trust-domain remedy and supported-range refusal.
- `crates/lash-sqlite-store/src/compat.rs:57-95` reads physical compatibility stamps.
- `crates/lash-sqlite-store/src/migration.rs:76-112` declares supported creation migrations.
- `crates/lash-sqlite-store/src/release_stamp.rs:1-115` defines release-stamp observations and updates.
- `crates/lash-postgres-store/src/postgres/release_stamp.rs:1-112` distinguishes writing and read-only opens.
