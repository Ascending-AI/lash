# Session ids are used once

## Status

Accepted.

## Context

Lash previously allowed a host to delete a session and create another session
with the same id. `IncarnationId` distinguished those lifetimes throughout
runtime state, history-node preimages, effect-journal identity, turn addresses,
and lifecycle retirement. Every seam that carried the discriminator was also a
seam that could omit it. Session-keyed deletion and await-event revocation
evidence were consequently unsafe when an id recurred.

Lash cannot prove that an arbitrary host string is globally unique. It can
enforce a narrower and sufficient invariant inside each store: once a
host-facing id has durably materialized session metadata in that store, the id
is never reused there. Deleting an id that never materialized is a no-op.

## Decision

A host-facing session id is host-provided, non-empty, identifies exactly one
session lifetime, and is used at most once in a store. Lash otherwise treats it
as an opaque UTF-8 string; host transports may impose narrower syntax and
length rules. Deleting a materialized host-facing id writes a permanent
tombstone. Creating or forking to a deleted id fails with
`StoreError::SessionDeleted`, whose message states that the id was used and
deleted. Retention and vacuum never remove this identity evidence.

Runtime-internal process session ids are lash-minted and hosts cannot address
them, but they are used once on the same terms: process pruning deletes them,
and a delete writes the same permanent tombstone. Pruning them without one was
tried and does not hold. The deleted set is not only a reuse fence, it is the
frontier a delete reads to reclaim tombstoned history rows whose owner is
already gone — a node can be tombstoned after its owner's delete, and no
session-scoped vacuum can reach it, because the owning id is unbindable either
way. An untombstoned process id therefore strands rows in the store forever.
Two rows of identity evidence per pruned process is the price of a store that
drains; the alternative was unbounded leaked history.

Creating an already-live id remains idempotent. Opening an existing id remains
an explicit operation. `fork_at` already takes the new host-provided session id
and uses the same permanent-tombstone admission path.

`SessionLifetime`, `EphemeralRunId`, and `IncarnationId` are removed. The old
durable/ephemeral identity distinction did not describe two identities after
reuse was forbidden; the meaningful boundary is whether a runtime has been
bound to a store. `SessionCommitStore::admit_and_bind_session` takes the
complete `SessionBinding` (id and relation), returns
`SessionAdmission::{Created, Rebound}`, materializes metadata, and checks both
the handle binding and permanent deletion fence atomically. The runtime reads
the materialized identity back before committing, so a loose third-party store
cannot silently alias another session.

Store-less sessions use their host-provided session id anywhere stable
derivation is required. They therefore require a distinct id for every session
within one process; `LashCore` rejects store-less id reuse for its process
lifetime. A durable effect host shared beyond that process requires the host to
provide uniqueness across that wider domain. Ordinary history nodes derive
from session id, operation id, and ordinal. Frame nodes derive from session id
and frame key. Effect-journal identities and turn addresses likewise use the
session id without a second discriminator.

SQLite schema 20, SQLite effect schema 6, and PostgreSQL schema 28 are
reject-and-recreate boundaries. No old shape is migrated or dual-read.
Await-event promise keys carry an explicit `v2` epoch. Old in-flight Restate
invocations cannot resume across this cutover: operators must drain them or
purge the Restate state before upgrading, otherwise their old promises are
orphaned under the prior key.

### Scope fences are a permanent-row class with one release rule (FIG-2499)

Process and runtime-operation scopes carry no session, so session deletion
never reaches their effect journal or their await-event promises. They are
reclaimed by scope-exact retirement instead: `EffectJournalRetirement::process`
and `::runtime_operation` delete the scope's effect rows, group rows, and
promise rows and write a scope fence (`effect_scope_retirements` /
`lash_effect_scope_retirements`, keyed by the scope's journal identity) in the
same transaction, under the same lock every admission path takes. Every
admission path — journal claim, group open, promise mint, resolve, peek, await,
and the in-process and Restate hosts' scoped controllers — reads the fence and
fails closed, so a late redrive can never re-execute under an emptied journal.
The fence is a permanent row on the same terms as `deleted_sessions`: retention
and vacuum never remove it, and the retention census lists it as permanently
exempt.

Retirement rests on proven unreachability, and the proof is named on the
request (`EffectRetirementGate`). A prune is owner-terminal proof: the registry
has deleted the row, so in-flight rows go too, and the facade fences exactly
the ids the registry's own eligibility survey returns — never a process the
registry keeps because of a projection watermark, a pending wake delivery, or
a parent-end plan. A receipt returning is not proof: the facade retires its
plugin-command and plugin-task scopes `when_quiescent`, and the store refuses
(`effect_scope_not_quiescent`) while a child is still in progress, a group
still waits for one, or a promise under the scope is still awaited and
unresolved (an `await_event_waits` row without a terminal; an open wait gate
on the in-process host; an indexed wait or live awakeable on Restate). A
refused retirement leaves the journal untouched and the facade simply
returns: it keeps no queue of deferred scopes, because that queue would die
with the process. The durable owner of deferred retirement is the reclaim
sweep (`SessionStoreFactory::reclaim_retained_evidence`, ADR 0067): under the
same fence lock it retires every session-free runtime-operation scope whose
owning operation has a recorded receipt and that is quiescent at sweep time,
and leaves any scope without a recorded receipt alone — no receipt, no proof.

A runtime-operation fence is permanent: those ids are used once. A process
fence lasts until the host registers the same id again. Host-named process ids
are reusable by contract, and the fence exists to cover the interval between
the prune and that re-registration; the registration lifts it. The lift
lives in the registry insert, not in any caller: every registrant — a direct
registration, `Processes::start`, a session-scoped start, a trigger delivery,
a tool-intent `StartProcess`, the Restate scheduler — ends in
`ProcessRegistrar::register_process_with_observers`, and that write deletes
the scope's `effect_scope_retirements` row in the same transaction as the
registry insert, or in the same critical section on the in-memory registry.

Each store has exactly one commit point per registration and one per
retirement, and the fence row lives where that commit point is. On
PostgreSQL both live in one database: the registration transaction deletes the
fence and inserts the record under the scope's advisory lock, and the
retirement transaction proves quiescence, inserts the fence, and deletes the
journal rows. On SQLite the process registry is its own file, and a
multi-database write that modifies more than one file commits per file, so
the process fence of a registered host lives in the registry file
(`effect_scope_retirements` in `PROCESS_SCHEMA`). Registration is then a
single-file transaction: fence delete and registry insert commit together or
not at all, so a crash leaves the id either fenced-and-unregistered or
registered-and-unfenced, never both and never neither. Retirement commits the
fence into the registry file first — the quiescence proof and the fence
insert are one transaction over the attached files under one `BEGIN
IMMEDIATE` — and only then purges the journal rows in a second transaction on
the journal file. A crash between the two leaves a fenced scope with stale
journal rows, which is safe: admission reads the fence from the registry file
(the effect host attaches it for reads once the registry is bound), so a cold
host over that journal admits nothing under the scope, and the leftover rows
are idempotent cleanup that the next host bind or reclaim sweep purges
(`purge_rows_under_fenced_scopes`). Runtime-operation fences, and process
fences written while no registry is bound, stay in the journal file and retire
in one transaction with their rows; when a registry binds it repairs the
journal file's leftovers — rows under any fence, and journal-file process
fences of ids the registry already holds — before the first admission.

The fence and the registry can also be two different stores. The in-process
host keeps a fence set, and Restate keeps a per-scope `LashDurableWaitIndex`
object that is revoked on retirement. Both bind to the registry
(`ProcessRegistrar::bind_effect_host`, done by `LashCore::build`), and the
binding runs both ways: the registry reinstates the host's fence from the same
seam once its insert has committed (`EffectHost::reinstate_effect_scope`
remains the host-facing lever that seam drives, and nothing else calls it),
and the host receives a `ProcessRegistryBinding` — the registry's own
"is this process registered" probe. On Restate that probe makes the index's
revoked flag a cache of the registry's truth rather than a second source of
it: a revoked index over a process the registry holds is a registration that
committed after its reinstate was lost (a crash between the insert and the
ingress call, or a reopen without the reinstate ever reaching the engine), and
the host's admission reads through to the registry, reinstates the index, and
admits — no explicit re-registration, on a SQLite- or PostgreSQL-backed
registry alike. The read-through was chosen over re-driving every registered
process's reinstate at bind time because binding is synchronous inside
`LashCore::build` and a bind-time scan is one ingress call per registered
process on every open; the read-through costs one registry probe per revoked
admission and nothing on the hot path.

Keying session-free waits by scope on Restate is a durable-wait identity
epoch cutover (epoch 5) and a tool-intent journal cutover (corpus v3):
pre-cutover state and journals refuse loudly before any effect re-executes;
the in-process host keeps an unbounded fence set for the same reason the
durable rows are permanent. Retention of these rows is a host lever on the
terms of ADR 0023.

Quiescence is measured on durable ground: an executing effect and a live
child of an open effect group count as live on every host, and on Restate
both are entries of the scope's `LashDurableWaitIndex` (`begin_effect` /
`end_effect` around every scoped effect the handler-side controller runs,
`record_group` when a group opens; a recorded group is live while its
`EffectGroupIndex` reports unsettled children). Memory waits are not durable:
a wait whose waiter was dropped before resolution stays a live entry in every
durable index and refuses retirement there, but the in-process host has no
record of a dropped waiter and retires the scope. That is the one
memory-versus-durable differential in the quiescence law, and it is stated
in the shared conformance law rather than papered over.

> **Historical versions.** The version numbers in this ADR record the state at ratification. The current values live in `lash::formats`; see `scripts/check_format_versions.py`.

## Consequences

- Deleting a session is final for that id in the store. A host reset creates a
  new id; it never deletes and reopens the old one.
- Permanent session-keyed await-event revocation is correct when the deletion
  tombstone, revocation ledger, effect journal, and Restate state share one
  lifecycle. They are one trust domain and must reset together. In particular,
  SQLite's catalog and effect database must not be wiped independently.
- Session-owned effect rows retire by session id. Process-owner incarnation
  fencing and replay-stream incarnation ids are separate concepts and remain.
- Hosts and third-party stores must implement the admission seam and preserve
  every deletion tombstone permanently — host-facing ids and lash-minted
  runtime-internal process session ids alike, whichever delete path wrote it.
  A store that keeps only the host-facing half satisfies the reuse fence but
  breaks reclaim: the delete arm reads the same set to decide which owners are
  gone. Lash detects reuse at creation rather than relying on every downstream
  identity preimage to carry a lifetime discriminator.
- A pruned process id is permanently unbindable as a session owner, and stays
  so after `compact_process_tombstones` removes the process tombstone that
  fenced its re-registration. Compaction frees registry rows, never ids:
  re-registering a compacted process id starts a process whose derived session
  stores cannot be created, failing with `StoreError::SessionDeleted` naming an
  internal id the host never chose (`process-env:<id>` or
  `process-session-turn:<id>`). Process ids are single-use for the store's life.
- Fork materialization followed by observer publication spans transaction
  domains. The fork relation retains the selected process ids as durable apply
  intent until every idempotent observer event commits. A crash burns no
  visibility choice: opening the single-use fork id replays the pending intent,
  and that id can never alias a later session lifetime. Replay reasserts the
  resolved selector wholesale: an observer removed before the intent is
  cleared can be added again, because clearing the durable host decision is
  the commit point.
