# Writing a table module for the SQL stores

How to add a table to the single-owner layout, and how to add a backend-only
statement to one that is already there. The reasoning behind the layout is
[ADR 0098](adr/0098-one-owner-per-sql-table-across-both-stores.md); this is the
procedure.

Every table family is converted (FIG-3387). Practically, that means a new
statement is not written at a call site: it is declared in a `statements!`
block in the family's owner module, or — if it names no table — in that
backend's connection module.

The worked example is the attachment family (FIG-3380):

| | shared | SQLite | PostgreSQL |
|---|---|---|---|
| attachment | `crates/lash-store-sql/src/attachment/{manifest,condemnation,blob}.rs` | `crates/lash-sqlite-store/src/{attachments,attachment_store}.rs` | `crates/lash-postgres-store/src/postgres/attachments.rs` |

## 1. Inventory the family first

Collect every production statement over the family's tables, in both backends,
before writing anything. Grep for the table name — and for its `lash_`-prefixed
spelling — across `crates/*/src`, not just the obvious module: the statements
this layout deletes are exactly the ones that ended up somewhere else. In the
effect family's conversion they had ended up in the retention sweep, the
process-registry registration path, the store's own `open`, and two
conformance helper binaries.

Then diff the two backends statement by statement and sort each one into:

* **identical after rendering** — placeholders, table prefix and schema
  qualifier are the only differences. This is shared.
* **anything else** — a `FOR UPDATE`, an `ON CONFLICT`, a server-clock
  expression, a boolean literal, a `RETURNING`. This is two backend-only
  statements.

Do not close a gap to make a statement shareable. Adding `ON CONFLICT DO
NOTHING` to a backend that does not need it turns a constraint error into a
silent no-op, which is a semantic change wearing a refactor's clothes. "Similar"
is not a membership class.

## 2. Write the shared table module

```rust
//! `attachment_manifest`: one row per attachment intent in a session.

/// The table's unprefixed name.
pub const TABLE: &str = "attachment_manifest";

/// Every column, in insert order.
pub const INSERT_COLUMNS: &str = "attachment_id, session_id, canonical_uri, …";

/// The intent as a caller reads it back.
///
/// The one lifecycle projection over this table, in the order callers decode
/// it.
pub const ENTRY_COLUMNS: &str = "attachment_id, session_id, canonical_uri, intent_at_ms, …";

lash_store_sql::statements! {
    /// `attachment_manifest` statements both backends issue verbatim.
    pub struct ManifestStatements @ "attachment_manifest" {
        /// Every digest the manifest still roots.
        select_rooted_ids = "SELECT DISTINCT attachment_id FROM attachment_manifest";
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
and compare it the same way, that struct belongs here. If the decoded row is
already a port type of the shared driver that consumes it — as the current
families' rows are — leave the type where the driver defines it and let this
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
than seventy-nine (FIG-2815, FIG-2844). A statement may not retype it.

So a neutral statement **names** the predicate, as a token:

```rust
lash_store_sql::statements! {
    pub struct NonTerminalPageStatements @ "process_non_terminal_page" {
        /// The non-terminal page, pinned to its partial index.
        count_live = "SELECT COUNT(*) FROM processes INDEXED BY idx_processes_non_terminal
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

static NON_TERMINAL_PAGE_SQL: LazyLock<NonTerminalPageStatements> = LazyLock::new(|| {
    NonTerminalPageStatements::render(Dialect::postgres().with_vocabulary(PROCESS_LIFECYCLE))
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
  statements, two owners. ADR 0098 rejects
  templating at fork points, and this does not reopen it.
* **It does not give the vocabulary a second source.** `lash-store-sql` has no
  `lash-core` dependency and no copy of any label. It holds the token; the
  enum still holds the words.

**Partial indexes are why byte identity matters.** `idx_processes_non_terminal`
is `ON processes(process_id) WHERE status IN ('running', 'waiting')`, and a
planner uses a partial index only for a query whose predicate matches it. The
token renders to exactly the schema's text — pinned per backend by
`every_vocabulary_partial_index_predicate_is_what_a_token_renders` in
`crates/lash-{sqlite,postgres}-store/src/process_lifecycle_sql_tests.rs`, which
also pins that a real non-terminal page statement renders to the bytes its `format!`
site produces today. Any family whose statements pin a partial index adds its
indexes to those tests.

## 4. Write each backend's backend-only set

Same macro, same family prefix, in the backend's table module:

```rust
lash_store_sql::statements! {
    /// `lash_attachment_manifest` statements only PostgreSQL issues.
    pub(crate) struct ManifestPostgresStatements @ "attachment_manifest" {
        /// Every uncommitted intent older than `?1`.
        ///
        /// The ordering is the fork: PostgreSQL reports digest order, SQLite
        /// reports oldest intent first. Both are total and neither caller
        /// depends on the other's, so the two orders are left exactly as they
        /// stand rather than unified inside a refactor.
        select_uncommitted = "SELECT attachment_id, session_id, canonical_uri, intent_at_ms,
                 committed_at_ms, owner_kind, owner_id, written_at_ms
             FROM attachment_manifest
             WHERE committed_at_ms IS NULL AND intent_at_ms <= ?1
             ORDER BY attachment_id ASC";
    }
}
```

The family prefix is **the same** on both sides on purpose: the shared and
backend-only names share one namespace, so a per-backend copy of a shared
statement's name is a collision, not a quiet override.

## 5. Render once, at startup

PostgreSQL has one dialect, so one `LazyLock`:

```rust
static ATTACHMENT_SQL: LazyLock<AttachmentSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    AttachmentSql { manifest: ManifestStatements::render(dialect), … }
});

pub(crate) fn attachment_sql() -> &'static AttachmentSql { &ATTACHMENT_SQL }
```

A SQLite family whose statements reach only its own database renders once
against that database's **schema** — `Schema::Main.dialect()` for a
session-catalog family, `Schema::ProcessRegistry.dialect()` for a registry
one. A family that reaches *across* databases renders each **deployment
layout** it can be issued under and picks at the call site:

```rust
static ATTACHMENT_SQL: LazyLock<AttachmentSql> = LazyLock::new(|| {
    let catalog = Dialect::sqlite(CATALOG);
    let beside_registry = Dialect::sqlite(CATALOG_BESIDE_REGISTRY);
    AttachmentSql {
        manifest: ManifestStatements::render(catalog),
        manifest_process_owner: ManifestProcessOwnerStatements::render(beside_registry),
        …
    }
});

pub(crate) fn attachment_sql() -> &'static AttachmentSql { &ATTACHMENT_SQL }
```

`Schema` (`crates/lash-sqlite-store/src/schema_layout.rs`) is `Main` or
`ProcessRegistry`, and each yields a `Dialect` whose layout places every owned
table in that one database (FIG-3406). A function that used to take a
`schema: &str` and `format!` its statement renders once and indexes. Call
sites read `sql.manifest.select_rooted_ids.sql()`; `.name()` is the
statement's reported name for tracing and store metrics.

### The schema is a property of the table, under a layout

A SQLite `Dialect` carries a [`TableLayout`]: an ordered list of the databases
this connection reaches and the tables each one holds. The renderer resolves
**each table name separately**, so one statement can join
`main.attachment_manifest` to `process_registry.processes`:

```rust
const CATALOG_TABLES: &[&str] = &[
    manifest::TABLE,
    condemnation::TABLE,
    "deleted_sessions",
    "graph_nodes",
    "runtime_turn_commits",
];

/// The session catalog with a bound process registry attached.
const CATALOG_BESIDE_REGISTRY: TableLayout = TableLayout::new(&[
    SchemaTables::new(Schema::Main.qualifier(), CATALOG_TABLES),
    SchemaTables::new(Schema::ProcessRegistry.qualifier(), &["processes"]),
]);
```

Three consequences worth knowing before you declare one:

* **A table the layout does not place is a startup refusal**, naming the table
  and the databases the layout does reach. That is a feature, not a hazard: the
  attachment family renders the probe that proves a process owner dead only
  under the layout that has a registry, so the statement a connection with no
  registry must not issue cannot be rendered for it at all. Two production
  shapes, two named statements, one layout each.
* **The first placement wins.** If a deployment carries copies of a table in
  two databases, each statement's layout chooses one copy by declaration
  order.
* **A layout may place every table the crate owns** — that is what
  `Schema::Main` does for a storage connection. It is still per-table
  resolution; the list is just total.

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

Two shapes worth knowing, both visible in the attachment family:

* A statement that was built per call because it needed a schema qualifier
  becomes a rendered statement per layout, picked by the caller. That is the
  whole point of rendering per `Schema` or `TableLayout` at startup.
* A statement that *cannot be rendered* under some layouts is a separate
  named statement set, not an optional predicate: the attachment family's
  `manifest_process_owner` statements exist only under the layout that binds
  a process registry, and a caller with no registry bound simply never reads
  them. The probe and the write that consults it already ran inside the
  caller's transaction, so the isolation is unchanged.

## 7. Prove it

```
kiln test //crates/lash-sqlite-store:all
kiln test //crates/lash-postgres-store:lash-postgres-store__unit_test
bash scripts/ci/with-service.sh pg16 -- bash scripts/ci/store-tests.sh pg-store
```

The two unit-test targets include `rendered_statement_sets_tests.rs`, which
forces every rendered statement set. Run them before the service-backed suite:
a statement that does not render fails there in one second, and in the
PostgreSQL suite as a poisoned `LazyLock` behind thirty other failures.

`pg-store` is the PostgreSQL conformance run. The suite is package-wide by
design — narrowing it to the conformance binary would silently drop the
integration and schema binaries — so there is no separate `conformance` suite to
ask for, and asking for one fails with `unknown store suite`.

The conformance and cross-backend suites are the oracle for "no behaviour
changed", and they pass **unedited**. If a suite needs a change to go green, the
change is the finding.
