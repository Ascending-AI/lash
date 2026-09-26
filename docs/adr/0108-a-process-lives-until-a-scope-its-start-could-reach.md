# 0108: A process lives until a scope its start could reach

## Status

Accepted 2026-09-26 (FIG-3607, PR-2: the lifetime cutover).

Supersedes the lifecycle vocabulary of
[ADR 0094](0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md)
(`ParentScope`, `OnParentEnd`, `ProcessLifecyclePolicy`). Its ledger survives
as the scope-close ledger, keyed by the scope that closed. Amends
[ADR 0107](0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md),
whose status deferred this slice.

## Context

ADR 0094 made a child's lifecycle a registration fact: a parent scope plus
`Cancel` or `Abandon`. Four problems remained.

- **Anyone could name any parent.** A declaration named its parent scope as
  data. Registration checked only that the scope's session matched the
  child's originator, so a start could attach itself to a scope it never ran
  under.
- **The host was a pseudo-scope that never ended.** `Host` meant "no parent"
  and "detached", and it was also a key the ledger had to refuse.
- **Turn ends were physical.** Every physical turn's commit wrote the turn's
  ledger row. A frame switch ended its physical turn while its logical root
  was still running, so `Cancel` children of the root were cancelled
  mid-root (FIG-3554).
- **Nothing ended a session.** Deleting a session left the processes started
  in it with nothing to settle them.

## Decision

### 1. Lifetime is `Until(scope)` or `Detached`

A start chooses its lifetime: `Lifetime::Until(ScopeRef)` or
`Lifetime::Detached`.

- A scope is a `ScopeId`: an opener (a turn root, a queued drain, a process)
  or a `Session`.
- A `ScopeRef` pairs the scope with the grant that let the start name it.
  - `Ancestor`: the scope is in the start's recorded ancestry.
  - `HostSessionLookup`: a host root start named a live session, which the
    facade looked up.
- `ScopeRef` has no public constructor besides `host_session_lookup` and no
  `Deserialize`. A declaration cannot invent one.
- The recorded, journaled form is `LifetimeDecision`, with the grant beside
  the scope.

### 2. Every start records its ancestry

The runtime materializes a `StartCx` from the admitted scope plus the
enclosing process's `ProcessLineage`. It records that context's `Ancestry`
on the registration (R1, R2).

- The ancestry lists the starter first, nearest first, and is empty for a
  root.
- A turn or drain start's ancestry is the opener, then its session.
- A process body's ancestry is the process, then the session it runs, then
  the process's own recorded ancestry.
- A context that lacks the lineage reads it back from the process's row.
  The lineage is an immutable recorded fact, never re-derived. With no row to
  read, the start is refused rather than recorded as a root.

### 3. Registration checks reachability, in every build

A registration is refused, typed, in three cases (R3):

- its `Until` scope under an `Ancestor` grant is not in its ancestry;
- a `HostSessionLookup` grant is on a start that is not a root, or names a
  scope other than a session;
- its session capability is outside its ancestry.

### 4. A policy decides, and the decision is journaled

A plugin that declares starts takes a required
`LifetimePolicy = Arc<dyn Fn(&StartCx) -> Lifetime>`. The policy runs at
declaration time. The chosen decision is journaled in the declaration, so a
redrive replays the decision rather than asking the policy again (R4b).

Helpers:

- `lifetime::session_or_starter` is process controls' usual choice.
- `lifetime::starter` is subagents' usual choice.
- `lifetime::detached`.

### 5. A scope closes once, after its end is durable

The scope-close ledger (`parent_end_plans`) holds one row per closed scope.

- **Process.** A process's terminal completion writes `Process(id)` in its
  own transaction.
- **Turn root.** A root's `Turn(root)` closes only after the root's terminal
  evidence is durable. The drive's recorded `CloseRootScope` step does this
  through the process registry's `RegistryScopeClose` adapter (R9). A frame
  switch writes no evidence, so the root's children survive it (FIG-3554).
- **Session.** `Session(s)` closes when the session's process state is
  deleted (R10).

The worker's recovery pass re-derives a root's missing row from the root's
terminal evidence.

### 6. A closed scope starts nothing

Registration refuses a start whose starter or whose `Until` scope has a close
row, in the same transaction as the check (R11). This covers `Detached`
starts too: a closed scope starts nothing.

## Consequences

- `ProcessRecord` gains `lifetime`, `ancestry` and `session_capability`, and
  the list filter is `until`.
- Durable surfaces change in place, with no version bump, under the pre-1.0
  version freeze (FIG-3846). Old state is refused, typed, not reinterpreted:
  - a parent-scope row's scope payload is not a `ScopeId`, so it is refused
    as malformed;
  - the SQLite process schema and the PostgreSQL component keep their
    stamps. A PostgreSQL catalog provisioned before the change fails the
    open-time shape check; a SQLite registry from before it fails its first
    process query on the missing lifetime columns. Either is recreated;
  - the effect journal, the Restate process journal and the remote protocol
    keep their generations; a start request's `RemoteStartLifetime` is
    `detached` or `until_session`, and a peer sending a `lifecycle` policy is
    refused.
- A start that must outlive its scope is `Detached`. There is no other way
  to escape a scope.
