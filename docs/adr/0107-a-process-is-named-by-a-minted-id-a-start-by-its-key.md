# 0107: A process is named by a minted id, a start by its key

## Status

Accepted 2026-09-25 (FIG-3607, PR-1: the identity cutover). The lifetime
vocabulary of [ADR 0094](0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md)
is unchanged by this slice except that its process arm names a `ProcessId`;
the lifetime rework (`Until`/`Detached`) and the retention guard are PR-2.

Amends [ADR 0094](0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md)
and [ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md)
§1: a process opener and a process parent scope are the minted id, and there
is no incarnation.

## Context

A process used to be named by a caller-chosen `ProcessId` plus a
store-assigned incarnation. The name did three jobs at once: it was the
idempotency key of the start, the address every later command used, and the
display name. Every reader had to carry the `(name, incarnation)` pair,
because a pruned name could be registered again and a bare name then
addressed the wrong lifetime. The pair leaked into handles, cursors, wake
deliveries, attachment owners, effect openers, trace keys and three wire
formats, and the registration fingerprint existed only to decide whether a
repeat under the same name was "the same" start.

It also broke recovery after prune (FIG-3611 L4/L5): a re-registered name
reused the pruned process's derived sessions (`process-env:{name}`,
`process-session-turn:{name}`), which prune had tombstoned, so the new
lifetime could not create them.

## Decision

### 1. A process id is minted, never chosen, never reused

`ProcessId` is `p_` followed by 32 lowercase hex digits of a UUIDv7. Only the
process registrar mints one, inside the transaction that registers the
process. There is no construction from an arbitrary string: an id is minted
or parsed back from bytes a registrar minted, and deserialization validates
the spelling. Every derived identity (the process's sessions, its effect
opener, its attachment owner, its trace graph key, its handle) is derived
from the minted id, so no two lifetimes can share one.

`ProcessIncarnation`, `ProcessRef`, `resolve_process_ref`, every `*_ref`
registry method, the `IncarnationSuperseded` refusals and the `Retired`
history retention are deleted. A pruned id refuses as
`ProcessNoLongerRetained`; an id no registrar minted refuses as
`ProcessUnknown`.

### 2. A start is keyed by an optional, trusted `StartKey`

`StartKey` is idempotency only. It is a framed digest (family
`lash.process-start-key` v1) over admitted operation identity, never over
submitted content, source or compiler identity, or the minted result. Each
start path has its own namespace:

1. a recorded tool intent: its replay key;
2. an orchestrating tool call: its admitted scope, call id and start ordinal;
3. a trigger delivery: occurrence, subscription, subscription incarnation and
   revision;
4. a host or remote caller: the caller's bytes;
5. a keyless host start: a fresh random key, so it is always new.

While a process registered under a key is retained, registration under the
same key returns that process (`Existing`) whatever the retry submitted. The
key is **trusted**: content is never compared. A changed-content retry adopts
none of its staged artifacts; they are released. After the process is pruned
the key starts a new process with a new id.

There is no name lookup. A host that wants a readable name sets the
registration's display label; nothing resolves a label to a process.

### 3. The start effect is addressed by the key

A journaled start is the effect `process:start:{start key}`; its staging
owner is keyed the same way. The minted id is a result the start records, and
a redrive reads it back. A journaled start without a key is refused
(`process_start_key_missing`).

### 4. A declared start answers a slot, not a handle

A tool attempt that declares a start cannot know the id. It answers
`{"__start_slot__": <intent index>}`; the attempt coordinator replaces the
slot with the handle of the realized start of that intent index before any
model or cell sees the output. A slot whose start did not realize is a typed
failure (`process_start_unrealized`), never a handle.

### 5. A trigger delivery binds its process after the start

A delivery reservation is unbound until its start registers; the router then
binds the minted id to the delivery. Recovery starts only unbound
reservations, under their delivery start key, so a crash between registration
and binding converges on the process the key already registered.

### 6. Clean cutover

Every persisted or wire shape that carried a name or an incarnation is bumped
and refuses its predecessor typed, before any effect: the SQLite and
PostgreSQL schemas, the remote protocol (window 100), the trace schema (35),
the Restate process journal, the effect journal, the process-command journal
payload, the tool-child request, the parent-scope payload, the wake-delivery
format, the lashlang segment state and the handle and cursor spellings
(`lashpc3`). There is no migration.

## Consequences

- **Known gap until PR-2**: there is no retention guard. A start whose result
  was lost **and** whose process was then pruned can double-start: the redrive
  finds no retained process under its key and registers a new one. Journal
  loss already refuses as `SubstrateLost`. The law "a redrive after prune
  returns the recorded id" holds whenever the start's recorded result
  survives.
- The fence-lift machinery that let a pruned name be registered again is no
  longer reachable from registration; it is removed with the scope fences in
  PR-2.
- Downstream hosts that looked processes up by name must switch to the minted
  `ProcessId` returned by the start, or to their own `StartKey`.
