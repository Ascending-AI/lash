# 0115: The 1.0 binary carries its half of every upgrade

## Status

Accepted. The compatibility machinery supports a one-release rolling upgrade
and rollback window. The pre-1.0 version freeze holds until the release cut;
synthetic-next provides executable successor coverage without changing the
default build's versions.

## Context

A rolling upgrade has two builds reading the same durable state and serving
calls to one another. The older build needs compatibility stamps, writer
fences, supported ranges and typed refusals before the newer build arrives.
Finalizing a release ends rollback and permits new writes and contraction.
A version stamp alone cannot protect an already-open writer or an actor that a
node of the other build claims.

The implementation separates component compatibility, fleet writer formats,
host wire contracts and the format set a node decodes
(`crates/lash-core-store/src/compat.rs`,
`crates/lash-core-store/src/store/fleet_format.rs`).

## Decision

| Number | Meaning | Authority |
|---|---|---|
| Component version and reader floor | The schema a build can open | PostgreSQL compatibility row; the SQLite database's compatibility row |
| `F` | The release compatibility epoch that selects durable writer formats | PostgreSQL `lash_fleet_format`; the SQLite database's `lash_compat` |
| Host wire contract | The message shape host clients exchange | Host-owned compatibility (§4) |
| Format set | The durable formats a node decodes | `lash_nodes.formats` and the claim filter of [ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md) §1 |

Before finalize, N+1 writes only what N reads. This includes semantics as well
as shape: retention, delivery, identity and ownership cannot require behavior
that N cannot preserve. A new build stamping an old number on incompatible
content violates the contract. Writer pins, admission and the synthetic
successor laws enforce the versioned parts of this rule
(`crates/lash-core-store/src/store/fleet_format.rs`,
`crates/lash-core-store/src/store/synthetic_next.rs`).

### 1. The component compatibility descriptor

#### 1.1 Types

`VersionRange` is a non-empty inclusive range. Construction and deserialization
reject zero and reversed bounds. Its frozen JSON shape is `{ "min": 1,
"max": 1 }`; `select` chooses the highest common version
(`crates/lash-sansio/src/compat.rs`).

A `CompatDescriptor` names a component and its read and write ranges.
`CompatStamp` carries `version` and `min_reader`. `DESCRIPTORS` lists
PostgreSQL and SQLite. Default component ranges are `[1,1]`. Synthetic-next
database components read `[1,2]` and write version 2
(`crates/lash-core-store/src/compat.rs`).
`lashctl version --json` exposes these declarations.

#### 1.2 Where the stamps live

PostgreSQL stores `(component, version, min_reader)` in
`lash_schema_versions` and `F` in `lash_fleet_format`. The release stamp
refuses pre-release state at the stable 1.0 boundary (§3.6); for released
stores it remains operator evidence
(`crates/lash-postgres-store/schema.sql`,
`crates/lash-postgres-store/src/postgres/schema.rs`).

The SQLite database stores `component`, `version`, `min_reader` and
`fleet_format` in its singleton `lash_compat` row. A SQLite deployment is one
database file and one transaction domain
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §12), so
it has one writer fence and one epoch. `lash_compat` is the admission
authority (`crates/lash-sqlite-store/src/schema.rs`,
`crates/lash-sqlite-store/src/compat.rs`).

#### 1.3 The admission rule

Before `admit` checks counters, `CompatRefusal::pre_release` applies the
release boundary in §3.6. Then `admit` checks in this order:

1. An unstamped empty component returns `Provision`; an unstamped populated
   component refuses `Unstamped`.
2. An unreadable stamp, zero floor or floor above its version refuses
   `MalformedStamp`.
3. A version below the declared read range refuses `TooOld`.
4. A floor above the range's maximum refuses `ReaderFloorAbove`.
5. A version inside the range is `Native`. A higher version with an admitted
   floor is `Expanded`.

The implementation is `crates/lash-core-store/src/compat.rs`.
PostgreSQL open verifies provisioning and refuses a `Provision` result;
workers apply no schema DDL. SQLite's open provisions or migrates before admission
(`crates/lash-postgres-store/src/postgres/schema.rs`,
`crates/lash-sqlite-store/src/backend.rs`,
`crates/lash-sqlite-store/src/compat.rs`).
`F` has separate admission against the build's writable range (§2.1).

#### 1.4 The shape check tolerates safe additions

An expanded catalog must preserve every definition the older build requires.
Additional tables, views and non-unique indexes are tolerable. An added column
on an expected table is tolerable when it is nullable or has a default and
has no extra write constraint. Additional required columns, checks, unique
constraints, foreign keys, exclusion constraints and triggers refuse
`ShapeRefused`. PostgreSQL's `NOT VALID` constraints still constrain new
writes, so they are unsafe additions.

PostgreSQL classifies the catalog findings for expanded admission; native
admission applies its configured shape check. `SchemaCheck::WarnOnly` can
relax native structural enforcement, but not compatibility or required seed
data (`crates/lash-postgres-store/src/postgres/schema.rs`,
`crates/lash-postgres-store/src/postgres/schema_shape.rs`).
SQLite checks expanded catalogs with `verify_tolerant`, while native
components need no additional expanded-catalog introspection
(`crates/lash-sqlite-store/src/compat.rs`).

#### 1.5 Typed refusals

Store compatibility errors use `StoreError::Incompatible { refusal }`.
Writer rejection uses `WriterFenced { recorded, writable }`.
`CompatRefusal` distinguishes absent or malformed stamps, old versions,
reader and writer floors, shape refusal, missing or unwritable fleet epochs,
pending SQLite migration, unknown vocabulary and pre-release state (§3.6).
Messages name operator remedies. Readable release evidence accompanies
applicable refusals
(`crates/lash-core-store/src/compat.rs`,
`crates/lash-core-store/src/store/error.rs`).

### 2. The fleet epoch `F` and the writer fence

#### 2.1 `F` is the release compatibility epoch

The default build writes epoch 1; synthetic-next writes under `[1,2]` and
owns epoch 2. Provisioning seeds the writable range's floor. Opening a store
never chooses or advances its epoch. An absent epoch is `FleetUnrecorded`;
an epoch outside the range is `FleetOutsideWritable`. Finalize moves `F` to
the finalizing build's own epoch, including a release without format changes
(`crates/lash-core-store/src/store/fleet_format.rs`).

`FleetFormat::writer_version` resolves the surface through `WRITER_PINS`.
Without a matching pin it uses the build's newest version. A finalize moves
the epoch rather than individually selecting every format
(`crates/lash-core-store/src/store/fleet_format.rs`).

A writer's epoch is its store's recorded one. Only a writer holding no store
uses the build's own epoch (`crates/lash-core/src/runtime/session_manager/mod.rs`,
`crates/lash-core/src/runtime/session_manager/process_runners/runner.rs`).
A payload validator admits the version each epoch in the writable range
assigns (`crates/lash-core-execution/src/runtime/process/effect_summary.rs`).

#### 2.2 The guarded transaction entry

Ordinary PostgreSQL mutations begin through `begin_guarded`. Its `BEGIN`
takes the fence's advisory lock shared, before session and row locks, and
its first data statement reads the fleet row. Finalize takes the same lock
exclusive before it moves the row. A writer already holding the shared lock
finishes before finalize; a later writer reads the moved epoch and refuses
before mutation if it cannot write it
(`crates/lash-postgres-store/src/postgres/guarded_tx.rs`,
`crates/lash-postgres-store/src/postgres/finalize.rs`). The lock is
transaction-scoped and writes nothing, so the fleet row takes no tuple lock
from the fleet's writers (FIG-5275).

Schema provisioning, migration and fleet-row control have explicit transaction
entries for their own lock order. The guarded-transactions check recognizes
these entries and the documented read-only exceptions
(`scripts/check-guarded-transactions.py`,
`crates/lash-postgres-store/src/postgres/migrate.rs`).

SQLite's `write` and `write_flow` run the component stamp and epoch fence
as the first statement after `BEGIN IMMEDIATE`. Its reserved writer lock
prevents another writer or finalize from changing that row until commit.
The same read re-admits a store that another process can migrate
(`crates/lash-sqlite-store/src/conn.rs`,
`crates/lash-sqlite-store/src/compat.rs`).

A migration or finalize acquires `BEGIN EXCLUSIVE` on the SQLite database,
then rewrites and commits in that one transaction
(`crates/lash-sqlite-store/src/compat.rs`).

#### 2.3 Pre-encoded commits

A commit encodes under the handle's last observed epoch. The transaction
compares that encoding epoch with its fence. A writable move rolls back,
rebuilds the plan and retries once; an unwritable move refuses `WriterFenced`.
The internal `FleetMoved` does not escape the store call
(`crates/lash-postgres-store/src/postgres/guarded_tx.rs`,
`crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs`).
The handle reports the epoch its fences observe, rather than only its opening
epoch (`guarded_tx.rs`, `crates/lash-sqlite-store/src/conn.rs`).

#### 2.4 Retry classes and whole-store recovery

| Failure | Action |
|---|---|
| Transaction contention | Retry the whole transaction, including admission and the fence. PostgreSQL's guarded helper bounds its local attempts. |
| A moved writable encoding epoch | Roll back, encode under the new epoch and retry once. |
| An unwritable epoch | Return terminal `WriterFenced` with no mutation. |
| Missing or malformed fence | Return a typed incompatibility and fail closed. |
| Interrupted SQLite migration | Resume the manifested catalog transition or finish its restore. |

PostgreSQL's fence and retry implementation is
`crates/lash-postgres-store/src/postgres/guarded_tx.rs`.
SQLite migration takes a migrator lock, checkpoints and closes the database,
durably backs it up, then applies the catalog under an exclusive lock. An
interrupted migration resumes from its manifest. A failure after the commit
restores the original backup, including when the failure occurs during a
resumed migration; open reports the failure. An interrupted restore completes
on the next open
(`crates/lash-sqlite-store/src/migration.rs`,
`crates/lash-sqlite-store/src/backend.rs`,
`crates/lash-sqlite-store/src/compat.rs`).

**SQLite finalize.** `SqliteStoreSet::finalize` checks that no live node
lacks the newer formats, under exclusive store ownership, and moves the epoch
in one transaction of the one database. There is no partial finalize to
recover. SQLite has no finalize hold because no fleet-wide automatic finalize
reaches a SQLite store
(`crates/lash-sqlite-store/src/backend.rs`,
`crates/lash-sqlite-store/src/finalize.rs`).

#### 2.5 Per-plugin writer ranges

The fleet record carries one writer range per plugin id beside `F`: the
format versions the fleet permits that plugin's state and config namespaces
to be published in. PostgreSQL keeps them in `lash_fleet_plugin_writers`,
read and written only under the `lash_fleet_format` row's lock; SQLite keeps
them in `lash_plugin_writers` in its one database
(`crates/lash-core-store/src/store/plugin_writers.rs`).

Every publication is described by the stamps of what it writes: a commit's
changed plugin-state component and recorded config, a created or forked
session's config, and a process execution environment's config. The guarded
transaction admits them before its first write, and a stamp outside its
plugin's range refuses `PluginWriterOutsideRange` with nothing published. A
malformed range refuses `PluginWriterRangeMalformed`. Process rows and
trigger targets hold an environment's content reference, never a namespace.

Ranges are provisioned from plugin registrations. Inside a rollback window a
provisioned plugin is permitted its oldest writable format; once `F` is the
provisioning build's own epoch it is permitted everything up to its native
format. A plugin the record does not name may publish its first format, which
records `[1,1]`; any other format refuses `PluginWriterUnprovisioned`.

Only finalize moves a recorded range, in the transaction that moves `F`: each
registered plugin's range rises to its native format and keeps its floor, so
history stays readable through the plugin's migrate steps. A recorded range
never contracts after finalize: a `[1,2]` range stays `[1,2]` rather than
contracting to `[2,2]` once the older format's writers drain, an accepted
simplification of the design's post-drain contraction (FIG-4858). An
admission already recorded under an older format may still write it; every
new admission selects the maximum common version (§2.6). A finalize that
would change a range while `F` already is the build's epoch refuses
`PluginRangesNeedEpochMove` and changes nothing.

`lashctl finalize --plugin-registrations <json-file>` passes the successor's
writer registrations into this same guarded flip. The file is the serialized
`PluginHost::composition()?.writer_registrations()` of the successor, an array
of `{plugin, native, writable}` declarations. The operator reads and validates
it before opening storage. Omit the file for a deployment with no plugin format
changes. Retained plugins absent from the successor keep their recorded ranges.

#### 2.6 Admissions record the plugin composition and writers

Every run admission and process start is a plugin adoption point and records
its choice
(`PluginAdmission`, in `crates/lash-core-store/src/store/plugin_writers.rs`):
the admitting build's plugins in hook order, each with its behaviour revision
and the writer format chosen for it. The writer is the highest format in both
the plugin's writable set and the range the fleet record permits when the
admission is made. A plugin that writes no permitted format refuses
`PluginWriterUnwritable` and nothing is admitted. This is the one place the
fleet record is read to choose a writer.

Two records carry it. A Run's admission records it on the run
(`RunAdmission.plugins`): the store keeps the first admission's choice and
answers it to every later admission of the run. A process's start records it
on `ProcessStarted.plugins`, and the process keeps that record for its life,
whichever node claims it. A process child therefore adopts the plugins of the
build that admits it and the ranges of the fleet at that moment.

Work admitted under a record writes plugin namespaces in the recorded
formats. A plugin session adopts the record, and a commit captures plugin
state, the session's sticky config and the running view's config in those
formats; a config transaction and a session creation choose the same way in
their own recorded step. A retry, resume or redrive reads the record back, so
a finalize that widens a range changes what the next admission chooses and
never what a recorded one writes. A session that has adopted no admission
writes each plugin's native format, which the guarded transaction still
checks against the range.

### 3. Nodes of two builds share the store

Nodes of N and N+1 run side by side over one store during a roll. They do not
call each other: every exchange between actors is a stored row (ADR 0132
§12), so compatibility is a matter of row formats, the format sets nodes
decode and the fleet epoch that selects writers.

#### 3.1 Every cross-build exchange is a stamped row

A row one build writes and another reads carries its format stamp. Readers
admit it through the surface's read range and registered lifts (§5); a stamp
outside the range refuses before typed decoding. Mailbox rows, wait rows and
Run records follow the same rule as any other guarded surface.

#### 3.2 The node's format set

Each node records the formats it decodes in `lash_nodes.formats`, and each
actor records the formats of its state in `lash_actors.formats`. A claim takes
only actors whose formats the claiming node decodes (ADR 0106 §1). An actor no
live node decodes stays visible and can be cancelled without decoding its
payload.

#### 3.3 The selected encoder

Writers choose each surface's format from `F` through `WRITER_PINS`. A newer
format is never written while a node of the older build is live; finalize
moves `F` only after the last such node has stopped (§8). A stale epoch view
writes the older readable format rather than advancing formats before the
SQL fence observes finalize.

#### 3.4 The versioned `RunOutcome`

A run's outcome is recorded under `LASH_TURN_OUTCOME_FORMAT_VERSION` in a
stamped envelope. It is immutable history and is never rewritten
(`crates/lash-core-store/src/store/fleet_format.rs`).

#### 3.5 Deployment and rollback routing

There is no routing: any node that decodes an actor's formats may claim it.
Before finalize N+1 writes only what N reads, so rollback starts N nodes and
stops N+1 nodes, and N claims every actor. A VM snapshot checks bytecode,
continuation, snapshot, accounting and ABI against component read ranges. An
unsupported component refuses with its name and range before decoding VM
bytes. Continuation and snapshot ranges follow their actual decoders
(`crates/lash-vm-protocol/src/contract.rs`,
`crates/lashlang/src/vm_contract.rs`). A snapshot outside a node's range is
never claimed by it.

Drain is by release (ADR 0106 §1). Finalize refuses while any live node lacks
the newer formats. An unreadable node table fails closed.

#### 3.6 The release line refuses pre-release state

A stable build at or above 1.0.0 refuses every store whose writing release
orders below 1.0.0, including `0.x` builds and `1.0.0-rc.1`, with
`CompatRefusal::PreRelease`. The refusal carries `component` and
`writing_release`; its JSON tag is `pre_release`. The shared rule in
`crates/lash-core-store/src/compat.rs` runs before, and regardless of, the
format-counter checks in preflight and store open. The store stays unchanged.
Frozen pre-release counters can match 1.0's baseline over incompatible shapes,
so counters cannot establish admission across this boundary.

A pre-release build opening a pre-release store keeps ordinary admission.
Released stores, and absent or unorderable release evidence, keep the normal
counter checks. An older released store is not pre-release state merely
because a newer released build opens it.

`pre_release` is the one documented signal to recreate the store once at the
1.0 cutover. Every other refusal stops the deploy. Pre-release state is never
migrated.

### 4. Host wire contracts

Hosts define their own transport DTOs and compatibility policy. Core Rust
types carry no wire-stability promise ([ADR 0136](0136-hosts-own-their-wire-contracts.md)).

### 5. Per-surface obligations

`GUARDED_SURFACES` records owner and lifetime policy for each guarded stored
format. `RECORD_UPCASTERS` has JSON tree lifts and `Decoder` markers for native
older-version decoders. A read window extends only through an unbroken lift
chain; it also admits the current fleet's pinned writer version. `F` does not
narrow supported history or mutable reads at finalize
(`crates/lash-core-store/src/store/fleet_format.rs`).

| Kind | Rule and implementation |
|---|---|
| Immutable history | Preserve bytes and hashes; retain the lift chain to its permanent floor. History includes checkpoints, session nodes, snapshots and LashTurn outcomes (`fleet_format.rs`). |
| Mutable rows | Write the fleet-selected format; keep reading old values during backfill (`fleet_format.rs`). |
| Derived workflow graph | Admit newest or fleet-pinned projection; regenerate an unsupported older projection from the module. It has no lift (`fleet_format.rs`). |
| Module artifacts | Store family and encoding in an envelope; verify under the stored supported family. Unknown family or encoding is `UnsupportedFamily`, rather than a hash mismatch (`crates/lashlang/src/artifact.rs`). |
| SQLite blobs | Store a versioned compression envelope. Unknown version or compression refuses without rewriting the bytes (`crates/lash-sqlite-store/src/codec.rs`). |
| Artifact and attachment referrers | Preserve canonical identities. Unknown kind is `Incompatible(UnknownVocabulary)`; malformed known identity is corruption (`crates/lash-core-store/src/artifact_referrer.rs`, `crates/lash-core-store/src/store/attachment_referrers.rs`). |
| Obligation vocabulary | Unknown state or kind is typed incompatibility. Delivery stalls undecodable work and keeps its row for inspection (`crates/lash-core-store/src/store/obligation.rs`). |
| Trace JSONL | Count and skip unknown event kinds. Malformed known events and unsupported schema versions refuse (`crates/lash-trace/src/jsonl_records.rs`). |
| Process cursors | Cursor minting uses the fleet-selected writer version; parsing rejects versions outside its readable range (`crates/lash-sansio/src/process_cursor.rs`, `crates/lash/src/process_observation.rs`). |
| Operator JSON | `lashctl` owns command DTOs and the `{schema_version, command, result, error}` envelope (`crates/lashctl/src/main.rs`). |

Format stamps are write metadata outside request-identity preimages.
Turn options project to their payload; checkpoint component content hashes
remain identity inputs. The commit planner stamps turn options under the
encoding epoch before hashing the commit. Heap readers derive allocation
identity and logical bytes from the allocation counter and objects. Pinning
identity to the first attempt's epoch would require a retry to discover
that epoch
(`crates/lash-core-store/src/store/runtime_commit_plan.rs`,
`crates/lash-core-store/src/store/runtime_commit.rs`,
`crates/lashlang/src/runtime/heap.rs`).

There is no universal unknown-field policy. Observational optional data can
be ignored where its decoder permits it; effect, ownership and identity
records use their typed admission rules. The compatibility window forbids
emitting semantics N cannot carry before finalize.

### 6. The synthetic N+1 gates

The upgrade harness builds the default and synthetic-next variants from one
tree. Synthetic-next moves every guarded surface, the component and fleet
ranges, cursor and format set, with old-format writer
pins and decoder coverage. Derived projections use regeneration instead of
lifts (`crates/lash-core-store/src/store/synthetic_next.rs`).

`crates/lash-upgrade-harness/tests/phase_a/main.rs` registers expanded-store
rollback, skipped-release refusal, writer/finalize races, host compatibility,
drain by release and rollback, retention and delivery rollback, history after finalize, workflow-graph range checks, and
the two-binary plugin writer-range rollback over SQLite file and PostgreSQL
overlap stores. Plugin rollback and retained-history unit laws also cover
SQLite memory and file stores, preserving recorded config and model routes.
`operator_json_contract` lives in `crates/lashctl/tests/`.

The store matrix is SQLite file, SQLite memory and PostgreSQL. Laws run the
production runtime over a fault-injecting store with labelled commits, a
virtual clock and `SimNodes` (ADR 0132 §14). Upgrade proofs use the
synthetic-next tier. The plugin rollback laws send admitted turns through the
production runtime. Other Phase A and rolling runs use the two node builds
against live PostgreSQL plus SQLite reopen cases; the
operator JSON proof needs one binary. `just phase-a` and `just e2e-rolling`
run the service proofs (`justfile:434`, `runbooks/rolling-upgrade/runbook.md:43`).
Each law's registration supplies its supported store and host combination.

`just e2e-rolling-cluster` runs the choreography under load on the Helm load
topology: PostgreSQL and kind, with N's and N+1's nodes side by side while
the load driver's sessions keep sending. It half-rolls, rolls back before
finalize, rolls, finalizes, and fences: the live N worker's write, a fresh N process and
N's operator are refused. The load verifier's witness classes judge no lost or
duplicated effects, stale writers fenced after finalize, the rollback
restoring N, and every session settling turns through each step
(`scripts/loadtest_upgrade.py`). It runs on
demand and sets no performance baseline.

### 7. Release-cut guardrails

Default versions stay frozen until the 1.0 cut. Pre-1.0 shape changes keep
the current numbers. Compatibility descriptors and refusal machinery are
part of the binary; numeric schema, protocol, cursor and VM constants retain
their current default values
(`crates/lash-sqlite-store/src/schema.rs`,
`crates/lash-sansio/src/process_cursor.rs`).
Synthetic-only changes do not advance those default versions.

### 8. Upgrade operations

The operator sequence is expand, roll, drain by release, finalize, then
finish backfills and contract. `lashctl` provides migrate, drain,
drain-status, end-drain, finalize, finalize-hold, preflight and version. Its exit codes are 0 done, 1 failure, 2 usage,
3 refused precondition, 4 incompatible store and 5 pending
(`crates/lashctl/src/main.rs`).

Finalize verifies that no live node lacks the newer formats, admits the epoch
under lock and moves it to `F_self`. PostgreSQL stores the operator hold on
the same fleet row. Automatic mode refuses a hold; `--override-hold` requests
manual mode. Rerunning a completed flip reports `already_finalized`
(`crates/lash-core-store/src/store/fleet_finalize.rs`,
`crates/lash-postgres-store/src/postgres/finalize.rs`).
SQLite finalizes its one database in one transaction and has no fleet-wide
automatic hold (`crates/lash-sqlite-store/src/backend.rs`).

Backfills require their declared finalized epoch. A batch advances its cursor
and rewritten rows in one guarded commit. Contract requires its epoch and
completed prerequisite backfills before it raises the reader floor
(`crates/lash-postgres-store/src/postgres/migrate.rs`).

## Alternatives and rationale

An unlocked epoch read permits a writer to commit after finalize under an
old format. The shared row lock closes that race. Checking PostgreSQL's reader
floor on every ordinary write adds another row read: contract requires
finalize, which already fences the excluded writers. SQLite checks the floor
at every write because it shares the epoch's local row.

A second SQLite stamp adds another authority. SQLite has one compatibility
row in its one database, and SQL owns fleet format selection.

Mutable unknown-field preservation is insufficient for new semantics; writer pins must keep N's semantics before
finalize. Immutable history needs permanent decoders because no backfill can
rewrite its identity-bearing bytes.

## Consequences

PostgreSQL runtime mutations pay an epoch read and shared row lock. SQLite
mutations pay a local compatibility read. An expanded compatible store remains
readable by the older build, while finalize fences its writers. Rollback needs
only nodes that decode the stored formats. Operators use one binary with
stable DTOs and exit codes. A compatibility release carries its predecessor's
readers, writer pins and recovery operations before it uses newer formats.
