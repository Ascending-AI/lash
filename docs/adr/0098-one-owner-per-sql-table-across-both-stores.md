# One owner per SQL table across both stores

## Status

Accepted.

## Context

SQLite and PostgreSQL share storage facts but differ in driver and SQL
contracts. Unnamed call-site queries duplicate column lists and make parity
depend on finding drift through behavior tests.

## 3. Decision

Each storage table has one owner for each fact. `lash-store-sql` owns the
unprefixed table name, named column projections, pure SQL row shapes, and
statements shared byte-for-byte after rendering. Each backend owns its driver
decoder and differing SQL. SQLite uses synchronous `rusqlite`; PostgreSQL uses
asynchronous `sqlx`.

Neutral statements use `?N` placeholders and bare table names. A tokenizer
renders placeholders, table names, and vocabulary tokens once per statement
set and deployment layout. It preserves literals, quoted identifiers, and
comments, and matches whole tokens. Unowned relations, malformed tokens,
unknown vocabulary, and missing table placements are render refusals.

Domain predicates such as `{{live_process_status(status)}}` come from the
backend's typed vocabulary. `lash-store-sql` has no `lash-core` dependency.
A vocabulary token does not permit a dialect-specific clause to appear shared.

Different locks, conflict clauses, boolean spellings, head tables, or database
operations remain named backend forks. Query shape changes have separate
statements rather than per-call string construction. Rendering does not alter
fencing, compare-and-set, or failure semantics.

Evidence: `crates/lash-store-sql/src/lib.rs:1`, `:102`, `:255`,
`crates/lash-store-sql/src/render.rs:1`, `:17`, `:143`, and
`crates/lash-core-store/src/store_backend_support/turn_input_lifecycle_sql.rs:18`.

## Column lists

Each table declares insertion columns and named read projections. Narrow reads
explain their omitted fields; call sites do not choose column subsets. A decoded
row already owned by a shared driver stays there. A pure SQL shape shared by
both backends belongs to the table module. Queued-batch settlement, for example,
reads only `admitted_root`, avoiding the unbounded authority envelope.

Evidence: `crates/lash-store-sql/src/turn_ingress/queued_batches.rs:12`, `:22`,
and `crates/lash-store-sql/src/process/events.rs:1`.

## 5. Deployment layouts

SQLite qualifies each table through `TableLayout`, whose ordered `SchemaTables`
entries name databases reached by the connection. Resolution uses the first
placement of that table. A standalone registry can use
`Dialect::sqlite_unqualified`; an attached registry and session catalog can
have different qualifiers in one statement. An absent placement refuses
rendering. PostgreSQL renders the `lash_` prefix. Table names and durable
encodings belong to their schema owners.

Evidence: `crates/lash-store-sql/src/render.rs:175`, `:201`,
`crates/lash-sqlite-store/src/turn_ingress.rs:181`, and
`crates/lash-postgres-store/src/postgres/session_sql.rs:12`.

## What is deliberately not adopted

- An ORM or generic driver connection broadens the abstraction into driver and
  transaction semantics beyond shared SQL text.
- Templating dialect forks obscures their differing contracts. Every fork has
  its own literal, name, and owner.
- Adding a conflict clause merely to share SQL changes observable failures.
- Renaming durable head tables for sharing invalidates stored data.

Schema artifacts and migration SQL have their own ownership. Migrations can
touch multiple table families. `schema_versions` stays schema-owned: its name
also names a release-stamp column, which token rewriting would prefix. SQL
without a table has a named backend owner.

## Consequences

Queries have owners and names. Shared SQL is written once; backend forks state
their differences. Rendered statement-set tests force sets and layouts, and
SQL pins check exact spelling.
[Store SQL authoring](../store-sql-authoring.md) gives the procedure.

Evidence: `crates/lash-sqlite-store/src/rendered_statement_sets_tests.rs`,
`crates/lash-postgres-store/src/rendered_statement_sets_tests.rs`, and
`crates/lash-store-sql/src/session/release_stamp.rs:14`.
