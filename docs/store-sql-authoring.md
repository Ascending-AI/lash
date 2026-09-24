# Writing a table module for the SQL stores

How to add a table to the single-owner layout, and how to add a dialect-only
statement to one that is already there. The reasoning behind the layout is
[ADR 0098](adr/0098-one-owner-per-sql-table-across-both-stores.md); this is the
procedure.

Every table family is converted (FIG-3387), so `scripts/check-store-sql-ownership.py`
is **total**: it reads both store crates whole, and there is no list of families
it is silent about. Practically, that means a new statement cannot be written at
a call site at all. It is declared in a `statements!` block in a listed owner
module, or — if it names no table — in that backend's connection module, and
anything else is a refusal naming the file and line.

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
* A relation the statement binds for itself — a `WITH … AS (…)` common table
  expression, a derived table's `) AS name` — is accepted in a `FROM` or `JOIN`
  and left unqualified. A `FROM` naming anything else is still a startup
  refusal, so a misspelled reference cannot reach a database.
* `FOR UPDATE` is a locking clause, not a statement head: the renderer does not
  read the word after it as a table, which is what lets `FOR UPDATE OF t` and
  `FOR UPDATE SKIP LOCKED` render at all.

One thing the neutral form deliberately cannot express: a statement whose text
depends on how many values it was given. A list bind is one bound value — a
JSON array unpacked with `json_each` on SQLite, a real array joined through
`unnest` or compared with `= ANY` on PostgreSQL — so the statement stays one
literal and only the value's length varies. The same rule kills the optional
predicate: where a filter varies, name one statement per filter *shape*
production callers actually use and pick it with an exhaustive match, because
neither `column = COALESCE(?N, column)` nor `?N IS NULL OR column = ?N` can use
an index. Add a query-plan test per named shape.

Row types: if both backends carry a byte-identical private struct for the row
and compare it the same way, that struct belongs here (see
`wait::waits::WaitRow`). If the decoded row is already a port type of the shared
driver that consumes it, leave the type where the driver defines it and let this
module own only the column order.

### One statement per filter shape, never one with an optional predicate

A read whose filter varies between call sites is **N named statements, one per
shape a production caller actually issues**, chosen by a small exhaustive
match. It is never one statement carrying `?2 IS NULL OR column <= ?2` or
`column = COALESCE(?2, column)`: neither predicate is sargable, so a planner
cannot use an index for *either* shape, and the read that had a filter pays for
the read that did not. The session graph is read whole and read up to a
generation ceiling; the preflight walk reads its first page and then pages
after a cursor; the retention sweep excludes a list of live keys or excludes
nothing. Each of those is two statements.

Each named shape gets a query-plan test — `EXPLAIN QUERY PLAN` on SQLite,
`EXPLAIN` on PostgreSQL — that pins the plan rather than asserting a property
of it. A pin makes the claim checkable in both directions: a change that turns
a seek into a scan moves the text, and so does an index that a shape *should*
now be using and is not. See
`crates/lash-sqlite-store/src/session_sql_tests.rs`.

### Common table expressions

A statement may bind relations with `WITH`, and read them back in table
positions: the session catalog, the readable-generation range and the
process-prune cascade are each one statement with a `WITH` clause, and each is
one statement precisely because its parts would race. The renderer reads the
clause, so the names it binds stand in a table position without being tables —
they carry no schema qualifier and no prefix, because nothing stores them.

One rule: **a bound relation may not be named after a table the crate owns.**
Both the definition and its uses would render to the same prefixed name as the
real table, so the statement would still run and a reader could no longer tell
which relation a position means. The renderer refuses it, naming the
expression.

## 3. Name domain vocabulary, never spell it

Some predicates are neither dialect nor prose. `status IN ('running',
'waiting')` is the *live process* partition, generated from `ProcessStatus` by
`lash_core_execution::store_backend_support` so that adding a variant is one edit rather
than seventy-nine (FIG-2815, FIG-2844). A statement may not retype it — the
`process_lifecycle_vocabulary` gate in `lash-sim` refuses that, and so does this
layout's own gate for a column a family declares vocabulary-valued.

So a neutral statement **names** the predicate, as a token:

```rust
lash_store_sql::statements! {
    pub struct WorklistStatements @ "process_worklist" {
        /// The live worklist's page, pinned to its partial index.
        count_live = "SELECT COUNT(*) FROM processes INDEXED BY idx_processes_live_worklist
     WHERE {{live_process_status(status)}}";
    }
}
```

`{{term(column)}}` is the whole grammar. `term` is a plain identifier; `column`
is a plain (`status`) or qualified (`processes.status`) identifier, because
that is what the vocabulary helpers take. Inner spacing is free. A token
inside a string literal or a comment is that literal's or comment's own text
and is left alone, exactly like a `?` inside `'why?'`.

The expansions come from the **backend** crate, which has the
`lash-core-execution` dependency this crate deliberately does not (ADR 0098). Each backend registers
them once, beside where it renders its statement set:

```rust
use lash_core_execution::store_backend_support as vocabulary;
use lash_store_sql::{Dialect, Vocabulary, VocabularyTerm};

const PROCESS_LIFECYCLE: Vocabulary = Vocabulary::new(&[
    VocabularyTerm::new(
        "live_process_status",
        vocabulary::live_process_status_predicate_sql,
    ),
    VocabularyTerm::new(
        "nonterminal_process_status",
        vocabulary::nonterminal_process_status_predicate_sql,
    ),
]);

static WORKLIST_SQL: LazyLock<WorklistStatements> = LazyLock::new(|| {
    WorklistStatements::render(Dialect::postgres().with_vocabulary(PROCESS_LIFECYCLE))
});
```

Term names are the same on both backends: the vocabulary is the domain's, not
a dialect's. Expansion happens once, at startup, in the tokenizer; nothing is
built per call. An unknown term, a dialect carrying no vocabulary, a malformed
token and a column that is not an identifier are all startup refusals naming
the term, not statements that reach a database.

Two rules about what this is **not**:

* **It is not a template mechanism for dialect forks.** A token names domain
  vocabulary that both backends spell identically and that is generated from
  one source. A statement whose text forks between backends is still two
  statements, two owners and a manifest entry each. ADR 0098 rejects
  templating at fork points, and this does not reopen it.
* **It does not give the vocabulary a second source.** `lash-store-sql` has no
  `lash-core` dependency and no copy of any label. It holds the token; the
  enum still holds the words.

Declare the vocabulary-valued columns in the family's manifest block, and the
gate refuses a statement that spells the vocabulary instead of naming it:

```toml
[families.process.vocabulary_columns]
processes = ["status"]
process_wake_deliveries = ["state"]
```

**Partial indexes are why byte identity matters.** `idx_processes_live_worklist`
is `ON processes(process_id) WHERE status IN ('running', 'waiting')`, and a
planner uses a partial index only for a query whose predicate matches it. The
token renders to exactly the schema's text — pinned per backend by
`every_vocabulary_partial_index_predicate_is_what_a_token_renders` in
`crates/lash-{sqlite,postgres}-store/src/process_lifecycle_sql_tests.rs`, which
also pins that a real worklist statement renders to the bytes its `format!`
site produces today. Any family whose statements pin a partial index adds its
indexes to those tests.

## 4. Write each backend's dialect-only set

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

## 5. Render once, at startup

PostgreSQL has one dialect, so one `LazyLock`:

```rust
static EFFECT_SQL: LazyLock<EffectSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    EffectSql { group: GroupStatements::render(dialect), … }
});

pub(crate) fn effect_sql() -> &'static EffectSql { &EFFECT_SQL }
```

SQLite reaches the same tables through more than one database, so it renders
one set per **deployment layout** and indexes:

```rust
static EFFECT_SQL: LazyLock<[EffectSql; 3]> = LazyLock::new(|| Schema::ALL.map(EffectSql::render));

pub(crate) fn effect_sql(schema: Schema) -> &'static EffectSql { &EFFECT_SQL[schema.index()] }
```

`Schema` (`crates/lash-sqlite-store/src/scope_fence.rs`) is `Main`,
`EffectJournal` or `ProcessRegistry`, and it *selects a layout* rather than
being a qualifier the dialect staples onto every table (FIG-3406). A function
that used to take a `schema: &str` and `format!` its statement takes a
`Schema` and indexes instead. Call sites read `sql.group.select_by_key.sql()`;
`.name()` is the statement's reported name for tracing and store metrics.

### The schema is a property of the table, under a layout

A SQLite `Dialect` carries a [`TableLayout`]: an ordered list of the databases
this connection reaches and the tables each one holds. The renderer resolves
**each table name separately**, so one statement can join
`main.attachment_manifest` to `process_registry.processes`:

```rust
const CATALOG_TABLES: &[&str] = &[manifest::TABLE, condemnation::TABLE, "deleted_sessions"];

/// The session catalog with a bound process registry attached.
const CATALOG_BESIDE_REGISTRY: TableLayout = TableLayout::new(&[
    SchemaTables::new("main", CATALOG_TABLES),
    SchemaTables::new("process_registry", &["processes"]),
]);
```

Three consequences worth knowing before you declare one:

* **A table the layout does not place is a startup refusal**, naming the table
  and the databases the layout does reach. That is a feature, not a hazard: the
  attachment family renders the probe that proves a process owner dead only
  under the layout that has a registry, so the statement a connection with no
  registry must not issue cannot be rendered for it at all. Two production
  shapes, two named statements, one layout each.
* **The first placement wins.** `effect_scope_retirements` lives in both the
  effect journal and a bound process registry (ADR 0049), so the layout that
  reaches the registry's copy is a different layout, not a second entry in the
  same one.
* **A layout may place every table the crate owns** — that is what
  `Schema::Main` is, and it is the truth for a deployment whose catalog and
  journal are one file. It is still per-table resolution; the list is just
  total.

A family that lives on **one** SQLite connection — the process registry's own
database — renders once with `Dialect::sqlite_unqualified()` instead, which
addresses its tables the way they have always been addressed. Keep that: the
rendered text is what a statement's `INDEXED BY` plan assertions were measured
against, and the layout machinery buys nothing for a family reached through one
name.

PostgreSQL has no layout: every table is `lash_<table>` in one database, so
`Dialect::postgres()` qualifies nothing. A statement whose SQLite half needs
two databases is still one shared statement, because the difference is a render
axis.

Attach the vocabulary (§3) to the dialect here, once, if the family's
statements use tokens: `Dialect::postgres().with_vocabulary(PROCESS_LIFECYCLE)`.

`render` panics on a malformed neutral statement, naming it. That runs once, at
first use, so the defect is a startup failure rather than a query that reaches a
database.

## 6. Move every call site, and delete what it replaced

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

## 7. Manifest every fork

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

`kind` is the short tag; `reason` is the prose. Both are required. The tag is
what makes the manifest countable — the per-reason census in ADR 0098 is summed
from it — so reuse an existing tag where one fits rather than inventing a synonym.

A new family gets a `[families.<name>]` block: its tables, its statement
prefixes, its shared, sqlite and postgres owner modules, its schema artifacts, a
`table_modules` map, and `vocabulary_columns` if any of its columns carry domain
vocabulary. A family is checked because it is declared there; there is no second
list to add it to.

### Adding a dialect-only statement to a family that is already there

Four edits, and the gate names each one you forget:

1. Declare it in that backend's owner module for the family, in a `statements!`
   block whose `@ "prefix"` is one of the family's statement prefixes. The
   neutral text uses `?N` even on PostgreSQL; the renderer rewrites it.
2. Add the field to the backend's rendered `…Sql` struct and its `render`
   constructor, so it is rendered once at startup rather than per call.
3. Add a `[[dialect_only]]` entry with `backends`, `kind` and `reason`.
   `backends` must match the declarations exactly — a statement only PostgreSQL
   has carries `backends = ["postgres"]`, and that entry *is* how "exists in one
   backend only" gets named.
4. Call it. A declared statement that nothing issues is refused: a statement set
   holds what the store sends, not what it might send.

If the statement projects two or more columns of the table, the projection must
already be one of the table module's column-list constants, or become one.

### Statements that name no table

Pragmas, `ATTACH`, advisory locks, isolation levels, the server clock and
catalog probes are SQL the table rules cannot see: they name no relation. They
have one home per backend, listed in the manifest:

```toml
[[connection]]
path = "crates/lash-postgres-store/src/postgres/connection_sql.rs"
reason = """
PostgreSQL's connection-scoped SQL: the advisory-lock shapes, the isolation
levels, the two `set_config` timeouts, the two clock reads and the `CHECK`
constraint catalog probe.
"""
```

Two rules hold that list to being a home rather than an exemption. A literal
there that is SQL over an owned table is refused — that statement belongs to
the table's family. And two literals with the same text in one store crate are
refused, which is the duplicate rule applied to the SQL the duplicate rule
cannot otherwise reach: before FIG-3387, `SELECT
pg_advisory_xact_lock(hashtextextended($1, 0))` existed six times verbatim
across six modules.

PostgreSQL's connection module is mostly a `statements!` set, so each one is
named for tracing and rendered once. SQLite's is plain constants, because a
pragma takes no bound parameter and no table name and there is nothing for the
renderer to rewrite.

Two shapes cannot be declared statements. The first is **a probe that reads a
system catalog in a table position.** `FROM pg_catalog.pg_constraint`
and `FROM sqlite_schema` name relations `lash-store-sql` does not own, and the
renderer refuses those — correctly, since a system catalog must never acquire
the `lash_` prefix. Such a probe is a plain constant in the connection module
and spells its own placeholders. The refusal is a `LazyLock` panic at first
use, so it is a startup failure rather than a bad query — and
`rendered_statement_sets_tests.rs` in each store crate forces every set so that
failure lands in the ordinary unit test rather than only in a service-backed
suite PR CI does not run.

The second is **a table whose name is also a column of another table.** The
renderer rewrites a table name wherever the token appears, not only in a table
position, so registering `schema_versions` would rewrite
`lash_release_stamp.schema_versions` too and every PostgreSQL open would fail.
That table stays with its schema artifacts, which the gate already lists.
Check a new table's name against the schemas' column names before adding it to
`TABLES`; there is exactly one such collision today and it is this one.

What does **not** belong there: anything that reaches a row lash stores. The
`lash_release_stamp` privilege probe reads a table, so it is a `release_stamp`
statement in the session-core family even though `has_table_privilege` takes
the relation as text — which is the one place the `lash_` prefix is spelled
rather than rendered, named as such in its manifest reason.

### Statements that span families

Some statements are genuinely over more than one family: the quiescence read
asks about effect rows, effect groups and promises in one breath, and
PostgreSQL's session delete is one CTE over twelve tables across three
families. Splitting them is not an option — the parts would race — so the rule
is ownership, not containment. **One owner module, one declaration, and the
other families' tables written down:**

```toml
[[cross_family]]
statement = "effect_journal.scope_is_quiescent"
owner = "crates/lash-store-sql/src/effect.rs"
touches = ["await_event_waits"]
reason = """
Quiescence is one question over both families; asking it as three statements
would let a child start between them.
"""
```

`touches` is exactly the set of owned tables outside the owner's family
that the statement is SQL over — the gate computes that set and compares, so
an entry cannot drift from the statement. A statement that reaches another
family with no entry is refused, and so is an entry for a statement that
reaches nothing. When your family converts, a statement of *another* family
that already reads your tables will appear here: that is where to look for it,
rather than in your own modules.

An `exempt` entry is not an alternative. It is for sources that are not
production runtime SQL at all — the deterministic-simulation reset, the
out-of-runtime runbook harness (`runbooks/restate-postgres-workers/`, a
subtree exemption: a path ending in `/`), and the three `testing`-feature
modules no production build compiles — and no production runtime statement may
be parked there.

## 8. Prove it

```
kiln test //crates/lash-sqlite-store:all
kiln test //crates/lash-postgres-store:lash-postgres-store__unit_test
python3 scripts/check-store-sql-ownership.py
python3 scripts/test_check_store_sql_ownership.py
bash scripts/ci/with-service.sh pg16 -- bash scripts/ci/store-tests.sh pg-store
```

The two unit-test targets include `rendered_statement_sets_tests.rs`, which
forces every rendered statement set. Run them before the service-backed suite:
a statement that does not render fails there in one second, and in the
PostgreSQL suite as a poisoned `LazyLock` behind thirty other failures.

`pg-store` is the PostgreSQL conformance run. The suite is package-wide by
design — narrowing it to the conformance binary would silently drop
`tests/attempt_atomicity.rs` — so there is no separate `conformance` suite to
ask for, and asking for one fails with `unknown store suite`.

The conformance and cross-backend suites are the oracle for "no behaviour
changed", and they pass **unedited**. If a suite needs a change to go green, the
change is the finding.

The gate's own suite seeds each violation it claims to catch and asserts the
refusal, against a copy of the real tree rather than a fixture. Any rule you add
to the gate gets the same treatment: a gate observed only passing proves
nothing.
