# One owner per SQL table across both stores

## Status

Accepted. Ratified on FIG-3380, which converts the effect and wait families and
builds the crate, the renderer, the manifest and the gate the rest of the arc
(FIG-3379) uses.

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

No path builds a statement per call. Where SQLite reaches a table through more
than one schema — the journal's own file as `main`, the same file `ATTACH`ed as
`effect_journal` for the retention sweep, a bound process registry as
`process_registry` — the statement set is rendered once per schema into a
three-element table and indexed, rather than `format!`ed at the call site.

**Every fork is a checked-in declaration.** `crates/lash-store-sql/dialect-only.toml`
lists each dialect-only statement, the backends that declare it, and why. A
statement declared in one backend only is the same kind of entry, which is what
makes "this operation exists on PostgreSQL and nowhere else" a fact somebody
wrote down rather than one somebody has to notice.

**A repo gate enforces all of it**, scoped by a `converted` list so it is total
for the families that have moved and silent about the rest until FIG-3387 closes
it. It refuses a production SQL literal over a converted table outside its owner
modules, a duplicated statement text within one backend, a shared statement
shadowed by a per-backend copy of its name, a dialect-only statement missing
from the manifest or listed for a backend that does not declare it, and a
projection of two or more columns that is not one of the column lists its table
module declares.

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

Also not adopted, and worth naming because it was close: **`lash-store-sql` has
no dependency on `lash-core`.** It is a leaf that owns SQL text, column lists
and the renderer. Where a table's decoded row is already a port type of the
shared driver that consumes it (`StoredEffectRow`, `EffectGroupRecord`), the
type stays with the driver and the column order that feeds it lives in the table
module — one owner per fact, not a parallel struct. Where the row is pure SQL
shape and both backends had privately declared a byte-identical copy of it
(`WaitRow` and its identity comparison), the shared crate takes it.

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

The gate is scoped, so it is honest about what it does not yet cover. Until
FIG-3387, a table outside `converted` may still be spelled anywhere; the gate
says nothing about it, rather than being weakened to let it pass.
