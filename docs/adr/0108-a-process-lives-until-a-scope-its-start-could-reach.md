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
- A turn or drain start's ancestry is the opener, then its session. When a
  process owns the session, the owner's lineage follows. A session's owner
  is the process whose start created it (a subagent's session), recorded
  once as `session_meta.owning_process_id`.
- A process body's ancestry is the process, then the session it runs, then
  the process's own recorded ancestry.
- A context that lacks the lineage reads it back from the process's row.
  The lineage is an immutable recorded fact, never re-derived. With no row to
  read, the start is refused rather than recorded as a root.
- A turn of an owned session that runs without the owner's live lineage (the
  host or the session's engine drives it) reads the owner from the session's
  metadata and the owner's lineage from the owner's row. An owner that
  retention has pruned ended long ago and bounds nothing, so the start
  records only its turn and session.

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
- **Session.** `Session(s)` closes through the session's `CloseSession`
  control intent, the one writer of that row (R10). The intent's engine half
  closes the session's open roots and then the session. Deleting the
  session's process state writes no close row.

A root's scope closes at least once, and never for a parked root, which has
no terminal evidence (L-C1). A crash between the terminal commit and the
close leaves the evidence durable and the scope open. The root's terminal
transaction arms the root row's scope-close obligation (ADR 0109), and the
close's own transaction delivers it. An engine that redelivers the
execution replays the root to its recorded close step; otherwise the relay
takes the due obligation and closes it. A delivered close is never
attempted again, and no pass rescans terminal roots.

Applying the close row's plan then owes each process living `Until` the
closed scope its cancel: the scope owner applies it when it can deliver, and
otherwise a later pass does (the process worker, or the reconcile tick's
parent-end slot, FIG-3822).

### 5a. The `CloseSession` intent is the deletion tombstone

A session's close is recorded as its `CloseSession` intent before anything
is deleted.

- **While it is open.** An open intent (pending, or failed and retryable) is
  what recovery works from. A crash between the intent and its
  acknowledgement leaves the intent open and listed for reconciliation. A
  retried deletion, or the engine's redelivery of it, answers the same
  intent and finishes its engine half (L-D6).
- **After it is acknowledged.** The intent is kept as the deleted session's
  tombstone. `root_terminal` answers every root of the deleted session from
  it as `Cancelled` with cause `SessionDeleted`. Once a root answers, it
  always does.
- **Retention.** Nothing prunes the tombstone. Like the session's identity
  tombstone in the deleted-session set, it is permanent deletion evidence,
  one row per deleted session. Evidence retention
  (`reclaim_retained_evidence`) and a session's deletion both leave it. A
  deletion removes only the session's other intents, the parked-root verbs.

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
  - the SQLite process schema, the SQLite core schema (`session_meta` gains
    `owning_process_id`) and the PostgreSQL component keep their stamps. A PostgreSQL catalog provisioned before the change fails the
    open-time shape check; a SQLite registry from before it fails its first
    process query on the missing lifetime columns. Either is recreated;
  - the effect journal, the Restate process journal and the remote protocol
    keep their generations; a start request's `RemoteStartLifetime` is
    `detached` or `until_session`, and a peer sending a `lifecycle` policy is
    refused.
- A start that must outlive its scope is `Detached`. There is no other way
  to escape a scope.

## Amendment (FIG-3562, 2026-09-29): declared starts and their cancellation

A `DeclaredStart` ([ADR 0116](0116-tools-are-opaque.md) §3) is a start like any other. Its lifetime is the
host policy's decision against the declaring attempt's `StartCx`, journaled
in the declaration, and `spawn_agent` uses `lifetime::starter`. Two
cancellations sit beside the lifetime without changing it. The parked call's
cancel obligation cancels the child when the call is cancelled or its
deadline elapses, and a scope close still cancels every `Until` child that
remains. A retention hold keeps the child's row until its consumer has
incorporated the result ([ADR 0116](0116-tools-are-opaque.md) §3.6).
