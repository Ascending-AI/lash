# Writing a table module for the SQL stores

How to convert one table family to the single-owner layout, or add a table to a
family that is already converted. The reasoning behind the layout is
[ADR 0098](adr/0098-one-owner-per-sql-table-across-both-stores.md); this is the
procedure.

The worked example is the effect and wait families (FIG-3380):

| | shared | SQLite | PostgreSQL |
|---|---|---|---|
| effect | `crates/lash-store-sql/src/effect{,/replay,/group,/scope_retirement}.rs` | `crates/lash-sqlite-store/src/{effect_replay,scope_fence}.rs` | `crates/lash-postgres-store/src/postgres/effect_replay.rs` |
| wait | `crates/lash-store-sql/src/wait/{waits,meta,revoked_sessions}.rs` | `crates/lash-sqlite-store/src/await_event.rs` | `crates/lash-postgres-store/src/await_event.rs` |

## 1. Inventory the family first

Collect every production statement over the family's tables, in both backends,
before writing anything. Grep for the table name — and for its `lash_`-prefixed
spelling — across `crates/*/src`, not just the obvious module: the statements
this layout deletes are exactly the ones that ended up somewhere else. In the
effect family they were in the retention sweep, the process-registry
registration path, the store's own `open`, and two conformance helper binaries.

Then diff the two backends statement by statement and sort each one into:

* **identical after rendering** — placeholders, table prefix and schema
  qualifier are the only differences. This is shared.
* **anything else** — a `FOR UPDATE`, an `ON CONFLICT`, a server-clock
  expression, a boolean literal, a `RETURNING`. This is two dialect-only
  statements with a manifest entry each.

Do not close a gap to make a statement shareable. Adding `ON CONFLICT DO
NOTHING` to a backend that does not need it turns a constraint error into a
silent no-op, which is a semantic change wearing a refactor's clothes. "Similar"
is not a membership class.

## 2. Write the shared table module

```rust
//! `runtime_effect_group`: one row per open effect group.

/// The table's unprefixed name.
pub const TABLE: &str = "runtime_effect_group";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "group_key, scope_id, …";

/// The group as a caller reads it back.
///
/// The one projection over this table, and the full row minus `next_seq`: the
/// counter is never read, only bumped and returned by the bump.
pub const RECORD_COLUMNS: &str = "group_key, scope_id, …";

lash_store_sql::statements! {
    /// `runtime_effect_group` statements both backends issue verbatim.
    pub struct GroupStatements @ "effect_group" {
        /// The durably recorded group row for `?1`.
        select_by_key = "SELECT group_key, scope_id, …
             FROM runtime_effect_group
             WHERE group_key = ?1";
    }
}
```

Rules the gate holds you to:

* Neutral SQL uses `?N` placeholders and bare, unprefixed table names. A `$N`
  is refused; so is a bare `?`.
* Every statement is **one string literal**. No `concat!`, no `format!`, no
  building. The gate parses these literals, and a call site that builds SQL is
  a call site that can drift.
* Every projection of two or more columns over the table must be one of the
  declared column-list constants, character for character modulo whitespace.
  Adding a column to a read means changing the constant, which means every
  reader of that projection changes with it.
* A **new** named projection is allowed, and must be documented where it is
  declared with why the full row will not do. "It is faster" is not a reason; a
  measurement, or a named unbounded column it avoids decoding, is.
* Add `TABLE` to `lash_store_sql::TABLES`, or the renderer will refuse any
  statement that names it.

Row types: if both backends carry a byte-identical private struct for the row
and compare it the same way, that struct belongs here (see
`wait::waits::WaitRow`). If the decoded row is already a port type of the shared
driver that consumes it, leave the type where the driver defines it and let this
module own only the column order.

## 3. Write each backend's dialect-only set

Same macro, same family prefix, in the backend's table module:

```rust
lash_store_sql::statements! {
    /// `runtime_effect_group` statements only PostgreSQL issues.
    pub(crate) struct GroupPostgresStatements @ "effect_group" {
        /// Record a group, returning the inserted row.
        ///
        /// The `RETURNING` clause is the fork: it saves the read-back on the
        /// insert path, which SQLite performs unconditionally.
        insert_new = "INSERT INTO runtime_effect_group ( … ) …";
    }
}
```

The family prefix is **the same** on both sides on purpose: the shared and
dialect-only names share one namespace, so a per-backend copy of a shared
statement's name is a collision the gate reports as shadowing rather than a
quiet override.

## 4. Render once, at startup

PostgreSQL has one dialect, so one `LazyLock`:

```rust
static EFFECT_SQL: LazyLock<EffectSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    EffectSql { group: GroupStatements::render(dialect), … }
});

pub(crate) fn effect_sql() -> &'static EffectSql { &EFFECT_SQL }
```

SQLite reaches the same tables through more than one schema, so it renders one
set per schema and indexes:

```rust
static EFFECT_SQL: LazyLock<[EffectSql; 3]> = LazyLock::new(|| Schema::ALL.map(EffectSql::render));

pub(crate) fn effect_sql(schema: Schema) -> &'static EffectSql { &EFFECT_SQL[schema.index()] }
```

`Schema` (`crates/lash-sqlite-store/src/scope_fence.rs`) is `Main`,
`EffectJournal` or `ProcessRegistry`. A function that used to take a
`schema: &str` and `format!` its statement takes a `Schema` and indexes
instead. Call sites read `sql.group.select_by_key.sql()`; `.name()` is the
statement's reported name for tracing and store metrics.

`render` panics on a malformed neutral statement, naming it. That runs once, at
first use, so the defect is a startup failure rather than a query that reaches a
database.

## 5. Move every call site, and delete what it replaced

Wholehog: no forwarding helper, no dual path, no interim layout. A helper whose
only job was to build the statement goes with it, and so do its tests. Tests
that duplicated a production statement use the named one; tests of a deleted
helper are deleted.

Two shapes worth knowing, both from the effect family:

* A statement that was built per call because it needed a schema qualifier
  becomes N rendered statements and an index. That is the whole point of the
  `Schema` parameter.
* A statement that was built per call because it needed a *variable number of
  locations* — SQLite's fence read disjoined one or two schemas into one
  `format!`ed predicate — becomes a short-circuiting loop over the rendered
  per-schema statement. Both reads already ran inside the caller's transaction,
  so the isolation is unchanged and the `OR`'s left-to-right short circuit
  becomes an early `return`.

## 6. Manifest every fork

One `[[dialect_only]]` entry per dialect-only statement in
`crates/lash-store-sql/dialect-only.toml`:

```toml
[[dialect_only]]
statement = "effect_group.insert_new"
backends = ["sqlite", "postgres"]
kind = "RETURNING"
reason = """
PostgreSQL returns the inserted group row from the insert, so the durable
read-back costs a second statement only on the conflict path. SQLite reads it
back unconditionally.
"""
```

`backends` must match the declarations exactly. A statement declared in one
backend only carries `backends = ["sqlite"]` and a reason that says why the
other backend has no such operation — that entry *is* how "exists in one backend
only" gets named.

Then add the family to `converted` and give it a `[families.<name>]` block: its
tables, its statement prefixes, its shared, sqlite and postgres owner modules,
its schema artifacts, and a `table_modules` map. Until the family is in
`converted` the gate is silent about it; once it is there the gate is total for
it.

## 7. Prove it

```
kiln test //crates/lash-sqlite-store:all
python3 scripts/check-store-sql-ownership.py
python3 scripts/test_check_store_sql_ownership.py
bash scripts/ci/with-service.sh pg16 -- bash scripts/ci/store-tests.sh conformance
```

The conformance and cross-backend suites are the oracle for "no behaviour
changed", and they pass **unedited**. If a suite needs a change to go green, the
change is the finding.

The gate's own suite seeds each violation it claims to catch and asserts the
refusal, against a copy of the real tree rather than a fixture. Any rule you add
to the gate gets the same treatment: a gate observed only passing proves
nothing.
