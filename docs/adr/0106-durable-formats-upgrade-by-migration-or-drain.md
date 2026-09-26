# 0106: Durable formats upgrade by migration or drain after the clean-slate release

## Status

Accepted 2026-09-25 (FIG-3660). Sam ruled the open questions Q1 to Q7 the
same day; they are recorded under *Rulings*. This record takes effect at the
clean-slate release, lash 1.0 (arc FIG-3789). Until then the temporary rule
holds: a durable-format change is a version bump plus a typed refusal of older
state.

At the release, this record replaces that rule wherever it is written as
current policy: ADR 0043 (*journal prefix*), ADR 0045 (FIG-3588), ADR 0105 §12,
and the docs of `state_version.rs`, `durable_wait.rs` (*drain and recreate*),
`lash::formats`, `durable_read_fixture.rs` and
`PostgresStorage::schema_version`. ADR 0105 §7 is amended separately to delete
the recorded `version()` op (Q6).

Implemented so far: the capture tool and the per-surface upgrade declaration
(FIG-3798, #2258). The rest lands in the order under §8.

The evidence is the policy draft and its critique
(`/workspace/notes/lash/fig3660-migration-policy.md`,
`fig3660-migration-policy/astra-report.md`), the peer check against
CockroachDB, Kubernetes, GitLab, Temporal and the expand/contract literature
(`fig3660-migration-policy/peer-check.md`), the two object-state reviews
(`/workspace/notes/lash/restate-object-state/{opus,astra}-report.md`) and the
FIG-3795 routing design (`/workspace/notes/lash/fig3795-design.md`).

## Context

figments upgrades by rolling deploys under live traffic. Old and new workers
share one PostgreSQL store and one Restate cluster. On `origin/main`
`074e55de9`:

- `versioned-surfaces.toml` registers 68 surfaces. Each `Comparable` decoder
  accepts exactly one version and refuses both older and newer versions.
  `OLDEST_SUPPORTED_SESSION_STATE_VERSION` exists, but admission still requires
  an exact match.
- PostgreSQL opens only when the component stamp (132) matches exactly.
  `SCHEMA_MIGRATIONS` exists, but no arm in it targets the current component.
  PostgreSQL and SQLite both reject a mismatched store and expect it to be
  recreated.
- `drain_status` counts across the whole store. Under a mixed fleet it never
  reads drained.
- `BuildGeneration` (ADR 0105 §7) is not used anywhere.
- Restate pins an invocation to its deployment once the invocation has started.
  A new invocation goes to the newest deployment that serves its service name,
  and that includes segment successors and group children of older state.
  Object state (the durable-wait index, the effect-group index and payload) is
  shared by every deployment, and the wait index refuses any state stamped
  with an older identity epoch.

## Decision

Every durable surface upgrades by one of three policies:

- **M, migrate:** schemas take expand/backfill/contract DDL (§5); payloads go
  through an upcaster chain on read.
- **D, drain:** a journal replays only under the code that wrote it, so it
  finishes on its own build (§1).
- **C, coexist:** both versions are live during the roll window.

Refusal survives only where §7 lists it.

### 1. Long-running work: the Temporal model

A process runs as a chain of segments. Each segment is **pinned to the build
that started it**, and the **next segment starts on the latest build**, with
its carried state converted forward at the hand-off. This is Temporal's
upgrade-on-continue-as-new. A segment's journal stays bounded by the segment
effect budget, and optionally by size, so a pinned segment always ends.

There are no patch markers and no replay of an old journal under new code:
ADR 0043 stands. There is no forced segment before each wait, no sleep
chunking, and no journal fork.

**Drain generation `G`** is a digest of the versions that give a journal its
meaning: the D rows of §4, plus a manual `JOURNAL_LOGIC_EPOCH` for logic-only
changes (FIG-3795). `BuildGeneration` carries it; the host passes it to the
Restate backend, and it is stamped on process rows, turn claims, parks,
handovers and group children.

**Routing.** Hosts keep registering one immutable Restate deployment per build
(ADR 0043). Every build binds each journal-bearing service twice:

- under its **stable name**, which Restate sends to the newest deployment.
  New processes and segment successors go here;
- under its **generation name** (`<Service>_g<G>`), which only builds of that
  generation serve. Effect-group dispatch and children go to the parent's `G`,
  a redrive goes to the route recorded for its segment, and a successor the
  latest build refuses goes to its writer's `G`.

The route is data stored beside the thing it routes (the handover row, the
group record, the admitted run). It is never recomputed from the caller's own
`G`, because Restate scopes workflow and idempotency keys by service name and
a recomputed name would run the work twice. Process terminal and attach always
use the stable root. The state-holding services (the durable-wait workflow and
the three objects of §3) are named once and never split by `G`.

**Generation sentinel.** Every journaling handler records the executing
build's `G` as its first journaled command. A replay that meets a different
`G` parks, typed, before any effect runs.

**Drain.** `drain_status(G)` reports per generation (Draining, then Drained).
It counts the invocations pinned to that generation's deployment, parked work
stamped with `G`, and queued successors, not store rows alone. The drain step
wakes waiting processes automatically: a waiting process is woken, hands over
to its next segment on the latest build, and re-waits there. This is the
counterpart of Temporal's broadcast signal, and it repeats until the
generation reads drained. The wake uses its own hand-off arm, never the cancel
arm, so the wait it leaves is not resolved as cancelled. Work that does not
drain before the timeout is reported by name, and the old deployment stays
registered until the operator settles it (§7).

**Required law.** A signal or event that arrives during a segment hand-off is
never lost and never delivered twice.

**A retired generation parks.** Work that reaches a generation no serving
build accepts parks, typed `RetiredGeneration` with its `G` (ADR 0045
`Parked`). It is never destroyed and never abandoned; the drain re-sends it to
the generation name while a build of that `G` serves.

### 2. Shared rows: the fleet format and finalize

**Fleet format version `F`** is one row in the PostgreSQL store (a meta row in
SQLite). It maps each migrated format to the writer version the fleet uses.

- Each build declares, per format, the versions it reads and the versions it
  can write. A new build reads both versions and writes the one `F` names.
  Features that need the new format stay off while it writes the old one.
- **Finalize** flips `F` to the new writer versions. It is the last step of
  the drain, and it runs automatically, once the old generation has drained to
  zero **and** its deployment is removed. It checks drained and retired
  deployments, never worker heartbeats, because a sleeping deployment can wake
  after its heartbeat expires. It fences stale writers in the same
  transaction.
- An operator **hold flag** stops the automatic finalize, like CockroachDB's
  `cluster.auto_upgrade.enabled = false`. With the hold set, the operator runs
  `lash finalize` by hand.
- **Rollback** to release N is safe until finalize, and after finalize the
  fleet only rolls forward.
- A build whose ranges do not admit the active `F` refuses to start, typed,
  before it takes traffic.
- SQLite runs in one process, so it migrates and finalizes on open.

### 3. Restate object state

The durable-wait registry, the effect-group state record and the effect-group
payload **stay in Restate**. Exclusive handlers give one writer per key, and
state writes commit with the journal; moving them to SQL would rebuild the
arbitration and wake scheduling that ADR 0104 §1 deletes.

- **Three version kinds, split.** Each object has a stored-value version (M),
  a handler-wire version (C: requests and responses accept N-1 shapes both
  ways, since old pinned callers reach new handlers), and a dispatch-journal
  version (D). `EFFECT_GROUP_INDEX_PROTOCOL_VERSION` splits into these three.
- **Versioned values.** Every stored value is versioned JSON; payload bytes
  carry a version stamp and keep their byte-equality fences. A read upcasts an
  N-1 value; before finalize, writers keep writing the old version.
- **Upgrade sweep.** Each object gets an exclusive `upgrade` handler that
  rewrites its values. A preflight enumerates keys and stamps through Restate
  SQL introspection. The sweep runs at finalize, and finalize also waits for
  zero old values. Introspection is eventually consistent, so it measures
  progress and never fences.
- **No namespacing.** Object keys (session id, journal identity, group key)
  are stable. When an inner key family changes, the same object reads entries
  under both names, each addressed from its own stored preimage; stored
  identities are never re-derived.
- **Deleted:** `DURABLE_WAIT_INDEX_IDENTITY_EPOCH`, its refusal gate, the
  drain-and-recreate doctrine, and the exact-version refusal on effect groups.
  Identity-family validation stays.
- **Renamed:** the Rust items — the `LashDurableWaitIndex` trait becomes
  `LashDurableWaitRegistry`, and `EffectGroupIndex` becomes `EffectGroupState`
  — while the registered Restate names stay `LashDurableWaitIndex` and
  `EffectGroupIndex` (FIG-3814).

### 4. Per-surface policy

`scripts/upgrade-paths.toml` declares the policy of every registered surface,
and `scripts/check_upgrade_paths.py` keeps it exhaustive against
`versioned-surfaces.toml`.

| Surface | Persisted in / read by | Policy |
|---|---|---|
| PG `SCHEMA_VERSION` | `lash_*` tables; every worker at open | **M** by the pre-deploy job (§5). |
| SQLite four `*SCHEMA_VERSION` | `user_version` per file | **M** on open, in one transaction, after a backup. |
| `CURRENT_SESSION_STATE_VERSION` | session marker; lease admission | **M** at rest: the session is upcast at admission. **D** in flight: a claimed or parked turn keeps its generation until it settles. The window is `[OLDEST_SUPPORTED, CURRENT]`. |
| `SESSION_HEAD_META`, `PROTOCOL_TURN_OPTIONS`, `SCOPE_STORAGE_PAYLOAD`, `PROCESS_WAKE_DELIVERY_FORMAT`, `NATIVE_DRIVER_STATE` | mutable rows; any worker | **M**. Upcast on read, write at `F`. |
| `SESSION_NODE_BODY`, `RUNTIME_COMMIT_RECEIPT`, `SESSION_CHECKPOINT`, `CHECKPOINT_COMPONENT_ENCODING`, `RLM_SNAPSHOT`, `LASHLANG_SNAPSHOT`, `HEAP_SIZE_SCHEDULE`, `NATIVE_TRANSPORT`, `PROCESS_EVENT_VOCABULARY` | immutable, hash-addressed history; replay and reopen | **M, read-only**. Upcast on read and never rewrite the stored bytes. These upcasters are permanent. |
| `WORKFLOW_GRAPH_SCHEMA`, `WORKFLOW_TYPE_FACET` | derived projection | **M**. Regenerate from the module. |
| `LASHLANG_SEMANTIC_HASH`, `BYTECODE_FORMAT`, four request-identity encodings, `*_FAMILY_VERSION`, `FRAME_KEY`, `JOURNAL_IDENTITY` | content addresses, idempotency keys | **C**. New identities are minted under the new family after finalize. A stored identity is never re-derived, and a retry is verified under the family it names. |
| `EFFECT_JOURNAL`, `RESTATE_PROCESS_JOURNAL`, `PROCESS_COMMAND_JOURNAL_PAYLOAD`, `DURABLE_WAIT_REQUEST`, `TOOL_CHILD_REQUEST`, `TOOL_SETTLEMENT`, `TOOL_ATTEMPT_CAPTURE`, `TOOL_PRESENTATION` | Restate journal and inputs; replay | **D** |
| `LASHLANG_CELL_JOURNAL_GRAMMAR`, `LASHLANG_REPLAY_KEY_GRAMMAR`, `INSTRUCTION_ACCOUNTING`, `LASHLANG_VM_ABI` | grammar the journals were written under | **D** |
| `TURN_CHECKPOINT_SCHEMA`, `VM_CONTINUATION_FORMAT`, `LASHLANG_SEGMENT_STATE` | parked turns and handovers (PG, Restate input) | **D** for the pinned segment; the handover is upcast into the next segment on the latest build. A handover outside the latest build's read window parks and routes to its writer's `G`. |
| `DURABLE_WAIT_INDEX_IDENTITY_EPOCH` | Restate object state | Deleted (§3). |
| `EFFECT_GROUP_INDEX_PROTOCOL_VERSION` | Restate object state, handler wire, dispatch journal | Split (§3): stored value **M**, handler wire **C**, dispatch journal **D**. |
| `REMOTE_PROTOCOL_VERSION`, `PROCESS_CURSOR_VERSION`, `TRACE_SCHEMA_VERSION` | live wire, host cursor, trace readers | **C**. Peers negotiate or accept `[N-1, N]`. |
| `PROCESS_LEASE_SCHEMA` | SQL engine lease | Deleted before the release (FIG-3667/3668). If it survives the release, it is **M**. |
| Durable-read fixtures, `tool_intent_journals/`, replay corpus | tests | Frozen at each release into `fixtures/release/<tag>/` (§6). |

Out of scope, because nothing durable carries them: `TOOL_CHILD_REBIND`,
`SOURCE_CACHE` and `QUEUED_WORK_CLAIM_LEASE_ENCODING`.

### 5. PostgreSQL schema changes

Schema changes run as a **pre-deploy `lash migrate` job**, never on worker
boot. lash ships the command and the migrations; the host owns the ordering.
The job takes the existing advisory lock, is idempotent and resumable, and
records every step in a `lash_migrations` ledger (migration id, phase,
release, state, timestamps).

| Phase | What runs | When |
|---|---|---|
| Expand | additive DDL | before the roll |
| Backfill | row rewrites, batched, resumable, idempotent | after finalize |
| Contract | drops | in the next release, only when the `F` row is finalized **and** the ledger shows the backfill done |

A row or semantic change never rewrites rows in place before finalize:

| Change | How |
|---|---|
| Value format inside a column | upcaster on read; an optional backfill rewrite after finalize |
| Column semantics (rename, split) | a new column or table, written by the new build after finalize, filled by the backfill; the old one drops at contract |
| Key change | new rows under the new key family; stored keys are never re-derived, and a rekey is a new table plus a backfill |
| Constraint | added `NOT VALID`, validated after the backfill |

**Workers never migrate.** Each checks a two-sided range: the schema stamp
carries its component and the oldest component that can read it, and a worker
opens the schema only when the schema is neither too old for it nor too new
for it. Anything outside the range refuses, typed, before the worker takes
traffic. Expand leaves the oldest reader unchanged, which is what lets N and
N+1 run side by side; contract raises it.

**Rollback** to release N before finalize is guaranteed, and a test proves
it. SQLite keeps migrating on open, after a backup.

### 6. Tests and gates

**Change lifecycle.** `check_version_bumps.py` still requires a bump for every
format change. The author of the bump also supplies the upgrade path:

- **M:** a migration with its phase, or an upcaster from the previous version.
- **D:** a `G` input (a D row or `JOURNAL_LOGIC_EPOCH`) and evidence that the
  handover from the old generation is inside the new build's read window, or a
  plan for the refused successors to drain on the old deployment.
- **C:** the dual-read window.

The release notes list the expand DDL, whether a finalize is needed, and
whether `G` changed.

**Upgrade laws.** Every surface has a law for every supported predecessor,
starting from a fixture frozen at the previous release tag by
`scripts/capture_release_fixtures.py`:

- **M and C:** new code reads or upcasts the fixture to the same public
  meaning. This extends the durable-read law.
- **D:** new code parks a journal from a foreign `G` with zero dispatch, and
  the release binary replays that journal to completion.
- **Hand-off:** a signal or event racing a segment hand-off is delivered
  exactly once (§1).
- **Rollback:** a store expanded and written by N+1 before finalize is served
  by N.

**Mixed-version rolling E2E.** This replaces `version-bump-recreation-e2e.sh`.
The harness brings up Restate, PostgreSQL, the release-tag image and head. It
seeds live turns, parked processes, effect groups and triggers on N, runs
`lash migrate`, rolls half the fleet under traffic and checks for no refusals,
no duplicate effects, and both builds reading each other's rows. It rolls back
and forward again, drains, retires the old deployment and finalizes. It runs
on every PR that touches a registered surface, and nightly.

**Gates.** `check_upgrade_paths.py` holds `upgrade-paths.toml` exhaustive
today. After 1.0 it also checks every bump: an M or C bump needs its
predecessor fixture plus an upcaster or migration from a registry a test
enumerates, and a D bump needs its `G` input. `check_version_bump_fixtures.py`
requires a migration from the release component, and a contract step may drop
an object only when no build in the window reads it.

**Deprecation.** A reader for an old format stays until the upgrade sweep or
backfill leaves zero old values and no retained release reads them. History
upcasters are permanent.

### 7. What stays fail-closed (typed)

- **A skipped compatibility release** (Q3 under *Rulings*), refused by the startup
  preflight.
- **A version newer than the build.** Finalize prevents this in normal
  operation.
- **A schema outside the worker's two-sided range** (§5).
- **Integrity failures:** hash, signature and divergence.
- **A journal whose `G` no build serves.** It parks (§1) and waits. It is
  never destroyed. Work the drain reports as stuck is settled by the operator:
  cancelled to a typed terminal state, or kept by leaving the old deployment
  registered.

### 8. Order

**Must land before 1.0** (milestone *lash 1.0*). These are obligations the 1.0
build must already honour, because 1.0 is the N-1 of the first upgrade:

1. **FIG-3795:** derive and stamp `G`, bind the stable and generation names,
   store routes as data, and add the generation sentinel.
2. **FIG-3796:** the `F` row contract and its enforcement: read both, write
   `F`, refuse an `F` outside the build's ranges.
3. **FIG-3797:** the two-sided PostgreSQL schema range instead of an exact
   match.
4. **FIG-3798:** the fixture capture tool and the per-surface upgrade
   declaration (landed, #2258); the 1.0 fixtures are captured at the tag.
5. **FIG-3799:** per-generation drain status, the automatic wake and hand-over,
   and the hand-off law.
6. **FIG-3814:** versioned values in Restate object state, the split version
   kinds, the deleted epoch refusals, and the renames.
7. **FIG-3816:** the `lash migrate` command (expand phase) and the
   `lash_migrations` ledger in the 1.0 schema.

**At the 1.0 cut** the pre-1.0 version freeze (FIG-3846) lifts in one change:

1. Remove the freeze switch — delete `freeze = "pre-1.0"` (and the `[policy]`
   table it sits in) from `scripts/versioned-surfaces.toml` — so every bump
   gate turns strict again: the findings it printed now fail.
2. Reset every guarded constant the registry names to its 1.0 baseline.
3. Start the migrate catalog (`EXPAND_MIGRATIONS` and the `lash_migrations`
   ledger) empty at the baseline.
4. Regenerate the fixtures that pin a generation: the recreation E2E's
   constants, the `COMPONENT_VERSION_PINS` literals, and the committed
   durable-read catalogs.
5. Prove strictness returned: a guarded-shape change without a bump fails
   `check_version_bumps.py` again. Gates whose derivations went stale while
   the freeze let shapes move under them — `check_version_bump_fixtures.py`
   still derives from the `SCHEMA_MIGRATIONS` catalog the expand-migrate
   model replaced — are repaired or retired in the same change; a gate that
   cannot evaluate fails strict mode.

**After 1.0**, under the operations arc FIG-3794, each before its first use:
FIG-3817 (backfill at finalize, the contract gate, tested rollback), FIG-3800
(`lash finalize`, the hold flag and writer fencing), FIG-3801 (the full
expand/contract runner), FIG-3802 (decoder read ranges and upcaster hooks),
FIG-3804 (remote-protocol negotiation), FIG-3805 (the rolling E2E), FIG-3806
(the operator guide), and the object `upgrade` handlers, sweep and
introspection preflight. FIG-3803 (namespaced object state) is superseded by
Q5.

## Rulings

Sam's rulings of 2026-09-25 (FIG-3660, logged on FIG-3622):

| Question | Ruling |
|---|---|
| Q1 Long-running work | The Temporal model: segments pinned to their build, the next segment on the latest build, per-generation drain with automatic wake and hand-over; no patch markers, forced segments, sleep chunking or fork; a retired generation parks; no signal lost or duplicated across a hand-off (§1). |
| Q2 Finalize | Explicit, automated as the last step of the drain, checked against drained and retired deployments, fencing stale writers, with an operator hold flag (§2). |
| Q3 Upgrade window | Only compatibility releases count. N supports N-1. A deployment further behind steps through the retained releases, one at a time; a skipped release is refused (§7). |
| Q4 Work that never drains | Superseded by Q1: there is no fork. The drain wakes and hands over waiting work; what remains parks (§1, §7). |
| Q5 Restate object state | Stays in Restate: split version kinds, versioned JSON with N-1 upcasters, read-both/write-old until finalize, introspection preflight and `upgrade`-handler sweep, no namespacing; the identity epoch and exact-version refusals are deleted; the Rust items rename to `LashDurableWaitRegistry` and `EffectGroupState` under the unchanged registered names (§3). |
| Q6 Recorded `version()` op | Deleted, with `ChangeId`, `VersionRange` and `RecordedVersion`; ADR 0105 §7 is amended separately. `DriveRequest.build_generation` stays. |
| Q7 PostgreSQL | A pre-deploy `lash migrate` job; expand, then backfill at finalize, then contract in the next release, gated on `F` and the ledger; the row-change table; a two-sided worker range; rollback before finalize guaranteed and tested; SQLite migrates on open after a backup (§5). |

## What is deliberately not adopted

- **Patch markers and replay under new code.** A journal replays only under
  its own code (ADR 0043). Temporal's patching API is the thing this avoids.
- **A journal-prefix fork.** Restate rejects restart-as-new for workflows and
  offers no service-name or input translation, so the fork could not move
  lash's process workflow onto a new generation.
- **Namespacing object state by version.** It splits one session's fence
  across namespaces and forces fan-out; stable keys with versioned values do
  not.
- **Moving object state to SQL.** No SQL transaction can span a Restate call,
  so every write would need its own idempotency, fencing and wake outbox.
- **Migrating on worker boot.** Workers of two releases share the store, so
  schema changes run once, locked, before the roll.
- **Heartbeat-gated finalize.** A heartbeat cannot prove that a deployment is
  gone.

## Consequences

- **Most format changes cost an upcaster and a fixture.** Changes to execution
  semantics also cost a drain window, and a changed `G` keeps the old
  deployment registered until the drain finishes.
- **Every release is an N-1 for the next.** The 1.0 build has to honour the
  stamps, ranges, ledger and routes of §8 before any upgrade exists to use
  them.
- **Rollback has a clear edge.** Before finalize, N serves the store; after
  it, the fleet rolls forward only.
- **Operators run three commands per release**: `lash migrate` before the
  roll, `lash drain` after it, and the automatic finalize (or `lash finalize`
  under a hold).
- **Risk: the hand-off law.** The Temporal model rests on hand-offs that lose
  and duplicate nothing; the law and the rolling E2E are the guard.
