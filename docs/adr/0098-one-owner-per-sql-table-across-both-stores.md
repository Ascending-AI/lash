# One owner per SQL table across both stores

## Status

Accepted. Ratified on FIG-3380, which converts the effect and wait families and
builds the crate, the renderer, the manifest and the gate the rest of the arc
(FIG-3379) uses. Amended by FIG-3399, which adds the vocabulary render axis,
cross-family ownership, and SQL structure to the gate's reading of a literal.
Amended by FIG-3406, which makes the SQLite schema a property of the table
under a deployment layout rather than of the statement. **Finalised by FIG-3387**,
which converts the last unowned tables, deletes the gate's `converted` list and
makes the gate total over both store crates. The arc's measured outcome is in
*Consequences*.

Amended 2026-09-23 (FIG-3540): [ADR 0101](0101-one-session-ingress-carries-every-admitted-item.md) deletes the
`pending_turn_inputs`, `queued_work_batches` and `queued_work_items` tables and
adds `session_ingress` (`lash_session_ingress`), owned by one table module in
the turn-ingress family. The head row gains the `pending_follow_on_json` column.
The ownership rules here apply unchanged.

## Context

Lash keeps two SQL stores. Measured at `b0b94f1f7`, the SQLite store held 368
production statements (336 distinct) and the PostgreSQL store 381 (347), over
about sixty tables, every one of them a raw string literal at its call site.
After normalising the table prefix, the placeholder style and the casts, 122 of
those statements were identical across the two backends and about 98 more were
at least 85% similar: roughly 65% of each backend is the same query written
twice. Each backend also carried about thirty verbatim duplicates of its own
statements, because nothing named a query, so the second author of a lookup had
no way to find the first.

The same rows were read through a dozen different column subsets — `processes`
through fifteen SELECTs, `graph_nodes` through fourteen — and two modules each
spelled `SELECT record_json FROM processes WHERE process_id = ?1` for
themselves. Parity between the backends was enforced only after the fact, by the
cross-backend conformance suite: a statement that drifted on one side was
caught, if it was caught, by a test noticing a different answer rather than by
anything noticing a different query.

A targeted prospect round read five dual-backend projects from local clones
(`/workspace/notes/lash/prospect-sql-dual-backend-2026-09-20.md`). Every one of
them reached the same conclusion independently: **nobody removed the second copy
of their precise queries.** Temporal's three SQL plugins are copy-paste forks
with visible residue (`$1` placeholders inside the SQLite plugin); River keeps
separate `.sql` sets per dialect and comments that its SQLite sqlc engine "is
not a little buggy, it's off the charts buggy"; Apalis dropped its shared-types
crate and its claim, lock and ack queries drifted apart semantically; Kine wrote
compaction three times. The payoff of sharing is real but confined: plain
statements, row types and column lists.

## Decision

Every table gets exactly one owner, split along the line the evidence draws.

**`lash-store-sql` owns, per table:** the unprefixed table name, one column-list
constant per *named* projection, the row types that are pure SQL shape, and
every statement whose text is byte-identical across both backends once
rendered.

**Each backend crate owns, per table:** its driver's row decoder — `rusqlite`
synchronous, `sqlx` async, both staying — and the statements that genuinely
fork.

**Membership is byte-identical after rendering, never "similar".** A statement
that differs by a `FOR UPDATE`, an `ON CONFLICT`, a server clock or a boolean
literal is two statements with two owners and a manifest entry each. Nothing is
templated, concatenated or string-surgeried at a fork point: the one mechanism
that would let "similar" statements share is exactly the mechanism that lets
them drift invisibly.

**Rendering happens once, at startup, through a tokenizer.** Neutral text is
written with `?N` placeholders and bare table names; the renderer rewrites
placeholders to `?N` or `$N` and table names to `main.<table>` or
`lash_<table>`. It understands string literals, quoted identifiers and
comments, so a `?` inside `'why?'` and a `$1` inside a `--` comment survive, and
it matches a table name as a whole token, so `await_event_waits` is not found
inside `await_event_waits_archive`. A regex gets all four of those wrong. It
also refuses a table position naming a relation the crate does not own, so a
statement that would run unprefixed against PostgreSQL fails at startup instead.

**Domain vocabulary is a render axis, beside the placeholder style and the
table prefix (FIG-3399).** Some predicates are neither dialect nor prose:
`status IN ('running', 'waiting')` is the live-process partition, generated
from `ProcessStatus` so that adding a variant is one edit and not seventy-nine
(FIG-2815, FIG-2844). A neutral statement names such a predicate as a token,
`{{live_process_status(status)}}`, and the renderer expands it once at
startup. The expansions are supplied by the **backend** crate, which has the
`lash-core` dependency this crate does not, so the vocabulary keeps exactly one
source. An unknown term, a dialect with no vocabulary, or a column that is not
a plain or qualified identifier is a startup refusal.

This is not the templating rejected below. A token renders a domain constant
the same way both backends render it, deterministically and identically — the
same class as the table prefix — where templating at a fork point exists to
make *different* texts look shared. The distinction is enforced, not asserted:
tokens name vocabulary, a text that forks is still two statements with two
owners and a manifest entry each, and the byte identity the mechanism depends
on is pinned per backend against the schema's own partial indexes, which a
planner only uses when the query's predicate matches them character for
character.

The alternative considered and rejected was to rewrite FIG-2844's gate from
"no store source spells a lifecycle literal" to "every spelled list equals what
the enum generates". That re-spells the vocabulary at about seventy-nine sites
and lints that they agree, which is FIG-2815 inverted.

No path builds a statement per call. Where SQLite reaches a table through more
than one schema — the journal's own file as `main`, the same file `ATTACH`ed as
`effect_journal` for the retention sweep, a bound process registry as
`process_registry` — the statement set is rendered once per layout into a
three-element table and indexed, rather than `format!`ed at the call site.

**On SQLite the schema qualifier is a property of the table, under a
deployment layout (FIG-3406).** A `Dialect` used to carry one qualifier for a
whole statement, which made a statement that joins two databases
unrenderable — and lash has such statements: the attachment manifest lives in
the session catalog while the `processes` rows that prove a process owner dead
live in an `ATTACH`ed registry, so the GC's live-root probe and its aged-intent
forget were still `format!`-built after the rest of their family converted. A
SQLite dialect now carries a `TableLayout`: an ordered list of the databases a
connection reaches and the tables each one holds. The renderer resolves every
table name through it separately, and a table the layout does not place is a
startup refusal naming the table and the databases the layout does reach.

That refusal is load-bearing rather than defensive. The two operations above
have two production shapes each — with and without a bound registry — and
"where a query's shape varies, name a statement per production shape" is
already this ADR's rule, because `COALESCE(?N, column)` and `?N IS NULL OR …`
are not sargable. The shape that proves a process owner dead is rendered only
under the layout that has a registry; the connection that has none cannot
render it at all, so the two shapes cannot be confused for one parameterised
statement.

What did **not** survive is the per-statement schema: nothing staples a
qualifier onto every table any more, and there is exactly one way a table
acquires one. What remains is rendering **once per layout**, indexed — which
is the same count of rendered sets as before, for the same reason it existed:
a SQLite connection reaches the journal's tables through more than one database
name, and the alternative is building the statement per call. A layout whose
table list is every table the crate owns is still per-table resolution; it is
the truth for a deployment whose catalog and journal are one file, and it is
why the effect and wait families render byte-identically across this change
(pinned, per statement and per layout, in
`crates/lash-{sqlite,postgres}-store/src/rendered_sql_pin_tests.rs`).

One table is in two databases at once: `effect_scope_retirements` is carried
by both the effect journal and a bound process registry, which is ADR 0049's
design and not an accident. A layout therefore places it in exactly one of
them, and the registry's copy is reached through a different layout —
`FenceLocations` already named those two locations and now selects between
them.

**Every fork is a checked-in declaration.** `crates/lash-store-sql/dialect-only.toml`
lists each dialect-only statement, the backends that declare it, and why. A
statement declared in one backend only is the same kind of entry, which is what
makes "this operation exists on PostgreSQL and nowhere else" a fact somebody
wrote down rather than one somebody has to notice.

**A statement that spans families has one owner and says so (FIG-3399).** Some
questions are genuinely over several families — quiescence reads effect rows,
effect groups and promises in one breath; PostgreSQL's session delete is one
CTE over twelve tables across three families — and splitting them would open
the window they exist to close. Such a statement is declared once, in one owner
module, with a `[[cross_family]]` entry naming the other families' tables it
reaches and why. The gate computes that set from the statement text and
compares, so the declaration cannot drift from the query, and a family's
converters find the other families' statements over their tables in one list
rather than by grepping.

**A repo gate enforces all of it, over both store crates whole (FIG-3387).**
There is no list of families it is silent about: a family is checked because it
is declared. It refuses a production SQL literal over an owned table outside its owner
modules, a duplicated statement text within one backend, a shared statement
shadowed by a per-backend copy of its name, a dialect-only statement missing
from the manifest or listed for a backend that does not declare it, a
projection of two or more columns that is not one of the column lists its table
module declares, an undeclared cross-family statement, and a statement that
spells a lifecycle literal over a column its family declares vocabulary-valued
— which is how this gate and FIG-2844's come to agree rather than merely not
collide.

FIG-3387 added the rules that make "total" mean what it says. A production
string literal anywhere under either store crate that *is* a SQL statement — it
opens with an upper-case SQL statement keyword — and has no declared home is
refused, which is what finally reaches the SQL the table rules are structurally
blind to: a pragma, an `ATTACH`, an advisory lock, an isolation level, a server
clock read, a catalog probe. Those get one **connection module** per backend,
listed with a reason, and held to two rules of their own: a literal there that
reaches an owned table is refused, because that statement belongs to the
table's family, and two with the same text in one store crate are refused,
which is the duplicate rule applied where it could not previously see. A
`[[dialect_only]]` entry must carry a short `kind` beside its prose, because a
census of the dialect-specific surface has to be summable. And a declared
statement that nothing issues is refused: a statement set holds what the store
sends, not what it might send.

Rendering is checked by a test rather than only at startup.
`crates/lash-{sqlite,postgres}-store/src/rendered_statement_sets_tests.rs`
forces every rendered set — and, on SQLite, every layout — so a statement the
renderer refuses fails the ordinary unit test. Without it the refusal is a
`LazyLock` panic at the set's first use, and for PostgreSQL that first use is
inside a service-backed suite per-PR CI does not run.

It reads a literal as SQL over a table only when the table stands in a relation
position, after `FROM`, `INTO`, `UPDATE`, `JOIN`, `TABLE` or `TRUNCATE`. A
statement keyword alone matched prose: a conformance test name about merging
wakes "across processes", the tool name `triggers.update`, the route
`/api/sessions/select`. Requiring structure is what lets the gate stay total
for a family without an exemption list of sentences.

**No schema, table name, durable encoding or version constant moves.** The
`lash_` prefix on PostgreSQL stays and is a render parameter exactly like the
placeholder style: renaming SQLite's tables would invalidate every existing
database for a cosmetic gain.

## Column lists

One column list per table is the rule, and the exception is named rather than
implicit. A table declares a full column list — the one its insert uses — and
may declare further **named** projections, each documented where it is declared
with why it is narrow. `runtime_effect_replay` has three and no full-row read,
because nothing reads the whole row: a claim decision, the drain's view of an
unranked child, and a settled member's report each drop a different column, and
`envelope_json` is unbounded in size. What is deleted is not narrow reads; it is
call sites picking their own columns.

## What is deliberately not adopted

* **sqlx `Any`.** No SQL translation, nine scalar type kinds, no arrays, no
  compile-time checking. Its one verified real user, SQLPage, is pinned to a
  fork of sqlx 0.6 and carries 88 dialect match sites anyway.
* **An ORM, or Diesel's `MultiConnection`.** Vaultwarden shares about 88% this
  way, but it has no lock-like query at all; 24 of its 27 remaining forks are
  one copy-pasted upsert.
* **`sea-query`, for anything safety-relevant.** Its `prepare_select_lock` is a
  silent no-op on SQLite ("SQLite doesn't supports row locking"), so a lock
  clause vanishes rather than failing. It also cannot express `INDEXED BY`,
  `unnest` or `json_each` without raw strings.
* **`sqlc`.** River's own source calls its SQLite engine off the charts buggy.
* **A Kine-style struct of overridable query fields.** It works at Kine's scale
  (one table, 27 slots) and has no completeness check: a backend that forgets to
  override inherits a statement that is wrong for it.
* **Claim logic in PostgreSQL stored functions**, as Apalis does. It moves the
  fork from the SQL to the database.
* **Templating or string surgery at a fork point**, including River-style
  runtime regex rewriting. This is the one mechanism that makes "similar"
  statements shareable, and it is the mechanism that makes a drift invisible.
  The vocabulary tokens added by FIG-3399 are not a way back in: a term names
  domain vocabulary that renders identically for both backends, never a
  dialect difference, and the gate refuses a statement that spells the
  vocabulary rather than naming it.

Also not adopted, and worth naming because it was close: **`lash-store-sql` has
no dependency on `lash-core`.** It is a leaf that owns SQL text, column lists
and the renderer. Where a table's decoded row is already a port type of the
shared driver that consumes it (`StoredEffectRow`, `EffectGroupRecord`), the
type stays with the driver and the column order that feeds it lives in the table
module — one owner per fact, not a parallel struct. Where the row is pure SQL
shape and both backends had privately declared a byte-identical copy of it
(`WaitRow` and its identity comparison), the shared crate takes it. The
vocabulary axis keeps that line: the crate holds the token, the backend supplies
the expansion, and the enum still holds the words.

## Consequences

The effect and wait families are converted in both backends and are the worked
example five more family lanes copy. `docs/store-sql-authoring.md` is the
procedure.

Fencing and compare-and-swap predicates are **not** touched here. Where one was
found in this family — PostgreSQL's `await_event_wait.resolve_pending`, which
carries the promise's identity comparison inside the `UPDATE` predicate because
`READ COMMITTED` cannot hold a read of it across statements, while SQLite
compares the row it read under its write lock — it is left exactly as it stands,
manifested as a CAS fork, for FIG-3381 to decide once in backend-neutral code.
That ticket is the one place the prospect round found a genuinely new idea worth
importing: Temporal decides fencing as lock-then-compare in shared Go code, with
the dialect contributing only a lock suffix.

### What the arc produced (FIG-3387)

Measured on the closing tree, against the `b0b94f1f7` baseline in *Context*.

| | baseline | now |
|---|---|---|
| SQLite production statements | 368 (336 distinct) | 399, all distinct |
| PostgreSQL production statements | 381 (347 distinct) | 410, all distinct |
| verbatim duplicates within a backend | ~30 each | **0 each**, enforced |
| identical across backends | 122 | **203**, shared and written once |
| statements naming no table | uncounted, unowned | 10 SQLite, 17 PostgreSQL, in one module each |
| tables with one owner | 0 | 55, across 8 families |

The surface did not shrink, and that is the honest headline. It grew by about
9% per backend while the duplicate count went to zero. Three things account for
the growth, and each is a statement that existed before and was not countable:
a read whose filter varied is now one named statement per production shape
rather than one `format!` with an appended predicate; a statement built per
schema qualifier is now one rendered set per layout; and the SQL that names no
table was not in the baseline's count at all because the baseline counted
statements over tables. What changed is not how much SQL there is but that
every piece of it has a name, an owner and a reason to differ.

Sharing roughly doubled: 203 of the statements each backend issues are one
text, written once, rendered twice — against 122 that merely happened to be
identical before. The prospect round's estimate that roughly 65% of each
backend was the same query written twice turned out to overstate what is
*shareable*: 203 of 399 is 51% on SQLite and 50% on PostgreSQL, and the
remainder is not laziness. It is named, per fork class, below.

One table is deliberately **not** registered, and it is a render-axis
constraint rather than an exception: `schema_versions` is the one lash table
whose name is also a *column* of another table
(`lash_release_stamp.schema_versions`). The renderer rewrites a table name
wherever the token appears, so registering it prefixes that column too and
every PostgreSQL open fails with `column "lash_schema_versions" of relation
"lash_release_stamp" does not exist`. Provisioning owns the stamp; its one read
is a named constant in `postgres/schema.rs`, which the gate already lists as a
schema artifact for every family. A future table whose name collides with a
column of another table has the same two choices — rename one of them, which
ADR 0098 freezes, or leave the table to its schema artifacts.

### The dialect-specific surface, by reason

279 `[[dialect_only]]` entries, 124 of them naming both backends (the same
operation, two texts) and 155 naming one (the operation exists on one backend
only). Counted by `kind` tag, with an entry contributing to each tag it
carries:

| count | fork class |
|---|---|
| 64 | operation exists on one backend only |
| 44 | head table differs (`session_head` / `sessions`, frozen above) |
| 37 | `FOR UPDATE` |
| 33 | boolean representation (`0`/`1` versus `FALSE`/`TRUE`) |
| 19 | `ON CONFLICT` versus `INSERT OR IGNORE`/`OR REPLACE` |
| 13 | JSON extraction (`json_extract` versus `->>`) |
| 12 | table exists on one backend only |
| 12 | server clock |
| 12 | list bind (`json_each` versus `unnest`/`= ANY`) |
| 9 | array parameter |
| 9 | durable shape (a fact stored differently, frozen) |
| 8 | `SKIP LOCKED` |
| 6 | `FOR SHARE` |
| 6 | bound id list |
| 5 | JSON pushdown |
| 4 each | scalar versus optional row; `INDEXED BY`; parameter cast; generated key |
| 3 each | `RETURNING`; bulk bind |
| 2 each | result ordering; `FOR KEY SHARE`; JSON assembly |
| 1 each | ten singletons, including the one remaining CAS predicate (FIG-3381) |

Two readings worth keeping. First, the single largest class — 64 — is not a
difference in how a query is spelled but an operation one backend simply does
not have, and the second largest, 44, is a table name ADR 0098 freezes. Neither
is addressable by any sharing mechanism. Second, the row-lock classes together
(`FOR UPDATE`, `FOR SHARE`, `FOR KEY SHARE`, `SKIP LOCKED`, 53 entries) are the
cost of PostgreSQL needing explicit locks where every SQLite write path already
holds `BEGIN IMMEDIATE`; that is the surface FIG-3381's decision layer would
have to shrink, and it shrank one of them.

Fifteen `[[cross_family]]` entries name the statements that genuinely span
families, and the gate recomputes each one's `touches` set from the statement
text, so an entry cannot drift from its query. Five `[[exempt]]` paths and two
`[[connection]]` modules are the whole of what is allowed to hold SQL without
owning a table.

### What we did not adopt, and why

Unchanged from *What is deliberately not adopted* above, and every reason was
verified against a local clone rather than taken from documentation
(`/workspace/notes/lash/prospect-sql-dual-backend-2026-09-20.md`): sqlx `Any`
(no SQL translation, nine scalar kinds, no arrays, no compile-time checks); an
ORM or Diesel `MultiConnection` (Vaultwarden shares 88% this way and has no
lock-like query at all); `sea-query` for anything safety-relevant (its lock
clause is a documented no-op on SQLite, so a lock vanishes instead of failing);
`sqlc` (River's own source calls its SQLite engine "off the charts buggy"); a
Kine-style struct of overridable query fields (no completeness check — a
backend that forgets an override inherits a statement that is wrong for it);
claim logic in PostgreSQL stored functions (moves the fork into the database);
and templating or string surgery at a fork point, which is the one mechanism
that makes "similar" statements shareable and the one that makes drift
invisible.

Two more the arc itself declined. **Closing a gap to make a statement
shareable** — adding `ON CONFLICT DO NOTHING` to the backend that does not need
it — was refused every time it came up: it turns a constraint error into a
silent no-op, which is a semantic change wearing a refactor's clothes.
**Renaming either durable head table** so the two backends could share their
head statements would have collapsed 44 forks at a stroke and invalidated every
existing database; the names stay frozen and the 44 entries are the price,
written down.
