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
A version stamp alone cannot protect an already-open writer or a journal
pinned to another deployment.

The implementation separates component compatibility, fleet writer formats,
wire negotiation and journal routing
(`crates/lash-core-store/src/compat.rs:54`,
`crates/lash-core-store/src/store/fleet_format.rs:30`,
`crates/lash-restate/src/compat.rs:41`,
`crates/lash-restate/src/engine.rs:263`).

## Decision

| Number | Meaning | Authority |
|---|---|---|
| Component version and reader floor | The schema or object family a build can open | PostgreSQL compatibility row, each SQLite compatibility row, each Restate object's `_compat` |
| `F` | The release compatibility epoch that selects durable writer formats | PostgreSQL `lash_fleet_format`; each SQLite database's `lash_compat` |
| Wire version | The message shape peers exchange | Remote negotiation or Restate's per-call envelope |
| `G` | The replay-equivalent generation that serves a journal | Recorded generation routes and deployment lanes |

Before finalize, N+1 writes only what N reads. This includes semantics as well
as shape: retention, delivery, identity and ownership cannot require behavior
that N cannot preserve. A new build stamping an old number on incompatible
content violates the contract. Writer pins, admission and the synthetic
successor laws enforce the versioned parts of this rule
(`crates/lash-core-store/src/store/fleet_format.rs:196`,
`crates/lash-core-store/src/store/synthetic_next.rs:1`).

### 1. The component compatibility descriptor

#### 1.1 Types

`VersionRange` is a non-empty inclusive range. Construction and deserialization
reject zero and reversed bounds. Its frozen JSON shape is `{ "min": 1,
"max": 1 }`; `select` chooses the highest common version
(`crates/lash-sansio/src/compat.rs:20`, `:63`, `:111`).

A `CompatDescriptor` names a component and its read and write ranges.
`CompatStamp` carries `version` and `min_reader`. `DESCRIPTORS` lists
PostgreSQL, SQLite core, process registry and triggers, and the three Restate
object families. Default component ranges are `[1,1]`. Synthetic-next database
components read `[1,2]` and write version 2; object families read and write
`[1,2]` (`crates/lash-core-store/src/compat.rs:54`, `:67`, `:151`).
`lashctl version --json` exposes these declarations.

#### 1.2 Where the stamps live

PostgreSQL stores `(component, version, min_reader)` in
`lash_schema_versions` and `F` in `lash_fleet_format`. A release stamp is
operator evidence, rather than an admission input
(`crates/lash-postgres-store/schema.sql:17`,
`crates/lash-postgres-store/src/postgres/schema.rs:30`, `:92`).

Each SQLite database stores `component`, `version`, `min_reader` and
`fleet_format` in its singleton `lash_compat` row. The three databases are
separate transaction domains, so each needs its own writer fence and epoch.
`lash_compat` is the admission authority
(`crates/lash-sqlite-store/src/schema.rs:41`, `:725`, `:1116`,
`crates/lash-sqlite-store/src/compat.rs:55`).
Restate object stamps use `_compat` (§3.2).

#### 1.3 The admission rule

`admit` checks in this order:

1. An unstamped empty component returns `Provision`; an unstamped populated
   component refuses `Unstamped`.
2. An unreadable stamp, zero floor or floor above its version refuses
   `MalformedStamp`.
3. A version below the declared read range refuses `TooOld`.
4. A floor above the range's maximum refuses `ReaderFloorAbove`.
5. A version inside the range is `Native`. A higher version with an admitted
   floor is `Expanded`.

The implementation is `crates/lash-core-store/src/compat.rs:391`.
PostgreSQL open verifies provisioning and refuses a `Provision` result;
workers apply no schema DDL. SQLite's whole-store open provisions or migrates
before component admission, and a component opened independently refuses a
pending migration
(`crates/lash-postgres-store/src/postgres/schema.rs:74`, `:117`,
`crates/lash-sqlite-store/src/backend.rs:244`,
`crates/lash-sqlite-store/src/compat.rs:316`).
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
data (`crates/lash-postgres-store/src/postgres/schema.rs:128`, `:151`, `:174`,
`crates/lash-postgres-store/src/postgres/schema_shape.rs:86`).
SQLite checks expanded catalogs with `verify_tolerant`, while native
components need no additional expanded-catalog introspection
(`crates/lash-sqlite-store/src/compat.rs:460`).

#### 1.5 Typed refusals

Store compatibility errors use `StoreError::Incompatible { refusal }`.
Writer rejection uses `WriterFenced { recorded, writable }`.
`CompatRefusal` distinguishes absent or malformed stamps, old versions,
reader and writer floors, shape refusal, missing or unwritable fleet epochs,
pending SQLite migration, partial store advancement and unknown vocabulary.
Messages name operator remedies. Readable release evidence accompanies
applicable refusals
(`crates/lash-core-store/src/compat.rs:189`, `:335`,
`crates/lash-core-store/src/store/error.rs:90`).

### 2. The fleet epoch `F` and the writer fence

#### 2.1 `F` is the release compatibility epoch

The default build writes epoch 1; synthetic-next writes under `[1,2]` and
owns epoch 2. Provisioning seeds the writable range's floor. Opening a store
never chooses or advances its epoch. An absent epoch is `FleetUnrecorded`;
an epoch outside the range is `FleetOutsideWritable`. Finalize moves `F` to
the finalizing build's own epoch, including a release without format changes
(`crates/lash-core-store/src/store/fleet_format.rs:30`, `:45`, `:123`,
`:143`, `:166`).

`FleetFormat::writer_version` resolves the surface through `WRITER_PINS`.
Without a matching pin it uses the build's newest version. A finalize moves
the epoch rather than individually selecting every format (`:196`).

A writer's epoch is its store's recorded one: a session's store, or for a
process run its process registry. Only a writer holding no store uses the
build's own epoch (`crates/lash-core/src/runtime/session_manager/mod.rs:130`,
`crates/lash-core/src/runtime/session_manager/process_runners/runner.rs:176`).
A payload validator admits the version each epoch in the writable range
assigns (`crates/lash-core-execution/src/runtime/process/effect_summary.rs:394`).

#### 2.2 The guarded transaction entry

Ordinary PostgreSQL mutations begin through `begin_guarded`. Its first
statement reads the fleet row `FOR SHARE`, before session and row locks.
Finalize locks the same row `FOR UPDATE`. A writer already holding the shared
lock finishes before finalize; a later writer reads the moved epoch and
refuses before mutation if it cannot write it
(`crates/lash-postgres-store/src/postgres/guarded_tx.rs:149`, `:185`,
`crates/lash-postgres-store/src/postgres/finalize.rs:105`).

Schema provisioning, migration and fleet-row control have explicit transaction
entries for their own lock order. The guarded-transactions check recognizes
these entries and the documented read-only exceptions
(`scripts/check-guarded-transactions.py`,
`crates/lash-postgres-store/src/postgres/migrate.rs:1568`).
The fence's PostgreSQL row lock needs `UPDATE` privilege on the fleet row.

SQLite's `write` and `write_flow` run the component stamp and epoch fence
as the first statement after `BEGIN IMMEDIATE`. Its reserved writer lock
prevents another writer or finalize from changing that row until commit.
The same read re-admits a component that another process can migrate
(`crates/lash-sqlite-store/src/conn.rs:783`, `:808`, `:841`,
`crates/lash-sqlite-store/src/compat.rs:144`).

A whole-store migration or finalize acquires `BEGIN EXCLUSIVE` on every
SQLite database in `SqliteDatabase::ALL` order, then rewrites and commits in
that order. Partial commits are detectable as a disagreeing set
(`crates/lash-sqlite-store/src/compat.rs:204`, `:215`).

#### 2.3 Pre-encoded commits

A commit encodes under the handle's last observed epoch. The transaction
compares that encoding epoch with its fence. A writable move rolls back,
rebuilds the plan and retries once; an unwritable move refuses `WriterFenced`.
The internal `FleetMoved` does not escape the store call
(`crates/lash-postgres-store/src/postgres/guarded_tx.rs:218`, `:236`,
`crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:1`).
The handle reports the epoch its fences observe, rather than only its opening
epoch (`guarded_tx.rs:99`, `crates/lash-sqlite-store/src/conn.rs:722`).

#### 2.4 Retry classes and whole-store recovery

| Failure | Action |
|---|---|
| Transaction contention | Retry the whole transaction, including admission and the fence. PostgreSQL's guarded helper bounds its local attempts. |
| A moved writable encoding epoch | Roll back, encode under the new epoch and retry once. |
| An unwritable epoch | Return terminal `WriterFenced` with no mutation. |
| Missing or malformed fence | Return a typed incompatibility and fail closed. |
| Partial SQLite migration | Resume the manifested catalog transition or finish its restore. |
| Partial SQLite finalize | Complete the sealed authorized transition; refuse an inconsistent set without that intent. |

PostgreSQL's fence and retry implementation is
`crates/lash-postgres-store/src/postgres/guarded_tx.rs:50`, `:149`, `:218`.
SQLite whole-store migration takes a migrator lock, checkpoints and closes
all components, durably backs up every file, then applies the catalog under
exclusive locks. An interrupted migration resumes from its manifest. A failure after any
component commits restores the whole original backup, including when the
failure occurs during a resumed migration; open reports the failure. An
interrupted restore completes on the next open. A component open never migrates independently
(`crates/lash-sqlite-store/src/migration.rs:1`,
`crates/lash-sqlite-store/src/backend.rs:244`,
`crates/lash-sqlite-store/src/compat.rs:316`).

**SQLite finalize recovery.** `SqliteStoreSet::finalize` checks generation
drain and deployment retirement under exclusive store ownership. For a
file-backed store, before the first database commit it durably seals an intent
naming the store, checked retirement, source and target epochs and all three
schema stamps. A fresh `SqliteStoreSet::open` whose build can write the target
epoch completes that authorized transition before migration or ordinary set
admission, under the ownership lock shared with migration and all three
exclusive database locks. Recovery requires no retained store handle. It
checks the recorded stamps and accepts only the source or target epoch in
each database; arbitrary inconsistent sets without that intent remain
refused. The intent is removed only after every epoch commit completes.
SQLite has no finalize hold because no fleet-wide automatic finalize reaches
a SQLite store
(`crates/lash-sqlite-store/src/backend.rs:234`, `:372`, `:382`,
`crates/lash-sqlite-store/src/finalize.rs:25`, `:49`, `:85`, `:147`, `:180`).

### 3. Restate

SQL cannot fence a Restate invocation atomically. Restate compatibility uses
per-call wire ranges, per-object floors, fleet-selected encoders and recorded
generation routes. Finalize also requires drain and deployment retirement.

#### 3.1 Every cross-build call carries the caller's range

Every Lash handler takes `Call<T> { wire, body }` and answers
`Reply<T> { wire, body }`. The frozen request range selects the highest
common `RESTATE_WIRE` version before typed body decoding; a disjoint range
refuses `lash.wire_unsupported` with both ranges. Replies carry the selected
version (`crates/lash-restate/src/compat.rs:41`, `:127`,
`crates/lash-restate/src/wire.rs:33`, `:137`).

Ingress may state the full readable range. A journaled call states exactly
the wire version its deployment's fleet epoch selects, so replay-equivalent
builds record the same call under that epoch
(`crates/lash-restate/src/compat.rs:49`, `:82`). A host handler's journaled
call reads the same epoch: the engine registers its store's fleet view when it
builds its endpoint. The host cache holds that store weakly and keeps its last
observed epoch after the store is released, so resource teardown cannot change
the bytes a host journal replays. A process that has never served a deployment
states its build's own epoch and warns `restate.host_wire_unbound` once
(`crates/lash-restate/src/compat.rs:110`, `:128`, `:152`,
`crates/lash-restate/src/engine.rs:210`).
Session and turn requests rely on this wire contract rather than a request
`drive_version` gate (`crates/lash-restate/src/session_driver.rs:788`, `:796`).
Journal generation remains a separate routing concern.

#### 3.2 The per-object `_compat` record

The effect-group index, effect-group payload and durable-wait index carry
`{ "format": 1, "min_reader": 1, "min_writer": 1 }` under `_compat`.
The record is separate from each value's `{format, body}` envelope
(`crates/lash-restate/src/compat.rs:140`, `:149`,
`crates/lash-restate/src/object_state.rs:53`, `:70`).

Handlers select wire compatibility before object admission. Reads check the
reader floor; mutations also check the writer floor. A fresh exclusive write
installs the record at its fleet-selected family format. A populated object
without it refuses `Unstamped`. Clear and retirement preserve the record,
so stale code cannot recreate state without admission
(`crates/lash-restate/src/object_state.rs:144`, `:170`, `:225`, `:277`).

An exclusive `upgrade` is `not_finalized` while the fleet selects an older
family writer format. Once it selects the newest format, the handler lifts
and rewrites older family values and raises `_compat` in the same invocation. Preflight and sweep find the
remaining object records through Restate SQL; another sweep resumes an
interrupted one without an external cursor
(`crates/lash-restate/src/object_state.rs:489`, `:542`,
`crates/lash-restate/src/object_upgrade.rs:175`, `:211`).

#### 3.3 The selected encoder

`StoredValueFormats::writer(fleet)` chooses the family writer from `F`.
`set_stamped` writes `{format, body}` at that chosen format. Readers admit
the recorded stamp through the supported lift chain and decode the lifted
body (`crates/lash-restate/src/object_state.rs:81`, `:368`, `:386`).
A stale epoch view writes the older readable format rather than advancing
formats before the SQL fence observes finalize.

#### 3.4 The versioned `RootOutcome`

`LashTurn` records its outcome under `LASH_TURN_OUTCOME_FORMAT_VERSION` in
the same stamped envelope. It is immutable history and has no object sweep.
The run, outcome and session-drive replies use the selected wire version
(`crates/lash-restate/src/session_driver.rs:71`, `:153`, `:788`, `:796`,
`crates/lash-core-store/src/store/fleet_format.rs:464`).

#### 3.5 Deployment and rollback routing

Each generation uses an immutable endpoint URI. Registration reads the
server's deployments: an unused URI registers without force; a held URI
serving this build's generation can redeploy with force; another generation
refuses `EndpointServesAnotherGeneration`
(`crates/lash-restate/src/engine.rs:263`, `:308`).
N and N+1 retain one namespace. Stable state-holding names share state;
generation names serve replay-equivalent journals. Recorded routes determine
where recovery goes. A generation sentinel guards a foreign journal before
execution (`crates/lash-restate/src/services.rs:44`,
`crates/lash-restate/src/process/workflow.rs:1075`).

Builds sharing `G` must replay the same journal steps. A change to their
logic, order, names or effects changes `JOURNAL_LOGIC_EPOCH`; `G` is not a
binary fingerprint. Opaque VM handover checks bytecode, continuation,
snapshot, accounting, heap schedule and ABI against component read ranges.
An unsupported component refuses with its name and range before decoding
VM bytes. Continuation, snapshot and heap ranges follow their actual decoders
(`crates/lash-vm-protocol/src/contract.rs:60`, `:71`,
`crates/lashlang/src/vm_contract.rs:13`, `:46`).
Work outside a receiving build's range retains its recorded generation route.

Rollback registers N at a fresh URI for new invocations and retains N+1's
pinned deployments until they drain. Generation drain requires its mark,
no live or parked processes, no parked or in-flight turns and no closing
sessions. Stalled obligations do not hold that generation drain; the operator
can inspect and resolve them separately. Finalize also refuses while any
registered deployment serves the retired generation. An unreadable deployment
registry fails closed
(`crates/lash-core-store/src/store/generation_drain.rs:214`,
`crates/lash-core-store/src/store/fleet_finalize.rs:182`).

### 4. Remote protocol negotiation

`Negotiation` carries unversioned, frozen `Hello`, `Accept` and `Unsupported`
messages. Peers select the highest common version; a disjoint range refuses
before executable message decoding. `Negotiated::from_accept` validates the
selection against both ranges
(`crates/lash-remote-protocol/src/negotiation.rs:41`, `:63`, `:96`).

`Envelope::at` uses that selection. `reply_to` preserves the request version.
`decode_json(bytes, local)` checks the envelope version before decoding its
typed body. Unsupported versions carry local and peer ranges, so each request
can be validated even by a peer that did not see the connection bootstrap
(`crates/lash-remote-protocol/src/lib.rs:332`, `:340`, `:442`, `:454`).
Hosts own transport establishment and negotiation. The default protocol
version is 100; synthetic-next exposes its successor range
(`crates/lash-remote-protocol/src/lib.rs:315`,
`crates/lash-remote-protocol/src/negotiation.rs:24`).

### 5. Per-surface obligations

`GUARDED_SURFACES` records owner and lifetime policy for each guarded stored
format. `RECORD_UPCASTERS` has JSON tree lifts and `Decoder` markers for native
older-version decoders. A read window extends only through an unbroken lift
chain; it also admits the current fleet's pinned writer version. `F` does not
narrow supported history or mutable reads at finalize
(`crates/lash-core-store/src/store/fleet_format.rs:215`, `:258`, `:297`,
`:358`, `:481`, `:491`).

| Kind | Rule and implementation |
|---|---|
| Immutable history | Preserve bytes and hashes; retain the lift chain to its permanent floor. History includes checkpoints, session nodes, snapshots and LashTurn outcomes (`fleet_format.rs:358`, `:464`). |
| Mutable rows and objects | Write the fleet-selected format; keep reading old values during backfill and sweep (`fleet_format.rs:310`, `:358`). |
| Derived workflow graph | Admit newest or fleet-pinned projection; regenerate an unsupported older projection from the module. It has no lift (`fleet_format.rs:234`, `:316`, `:424`). |
| Module artifacts | Store family and encoding in an envelope; verify under the stored supported family. Unknown family or encoding is `UnsupportedFamily`, rather than a hash mismatch (`crates/lashlang/src/artifact.rs:364`, `:410`, `:457`). |
| SQLite blobs | Store a versioned compression envelope. Unknown version or compression refuses without rewriting the bytes (`crates/lash-sqlite-store/src/codec.rs:110`, `:141`). |
| Artifact and attachment referrers | Preserve canonical identities. Unknown kind is `Incompatible(UnknownVocabulary)`; malformed known identity is corruption (`crates/lash-core-store/src/artifact_referrer.rs:1`, `crates/lash-core-store/src/store/attachment_referrers.rs:321`). |
| Obligation vocabulary | Unknown state or kind is typed incompatibility. Delivery stalls undecodable work and keeps its row for inspection (`crates/lash-core-store/src/store/obligation.rs:529`, `crates/lash-core-execution/src/runtime/drive/relay.rs:288`). |
| Trace JSONL | Count and skip unknown event kinds. Malformed known events and unsupported schema versions refuse (`crates/lash-trace/src/jsonl_records.rs:84`). |
| Process cursors | Cursor minting uses the fleet-selected writer version; parsing rejects versions outside its readable range (`crates/lash-sansio/src/process_cursor.rs:25`, `:142`, `:167`, `crates/lash/src/process_observation.rs:911`). |
| Operator JSON | `lashctl` owns command DTOs and the `{schema_version, command, result, error}` envelope (`crates/lashctl/src/main.rs:26`, `:874`). |

Format stamps are write metadata outside request-identity preimages.
Turn options project to their payload; checkpoint component content hashes
remain identity inputs. The commit planner stamps turn options under the
encoding epoch before hashing the commit. Heap writers choose their schedule
stamp at encoding. Pinning identity to the first attempt's generation would
require a retry to discover that generation
(`crates/lash-core-store/src/store/identity_projection.rs:1`,
`crates/lash-core-store/src/store/runtime_commit_plan.rs:146`,
`crates/lash-core-store/src/store/runtime_commit.rs:1158`,
`crates/lashlang/src/runtime/heap.rs:57`).

There is no universal unknown-field policy. Observational optional data can
be ignored where its decoder permits it; effect, ownership and identity
records use their typed admission rules. The compatibility window forbids
emitting semantics N cannot carry before finalize.

### 6. The synthetic N+1 gates

The upgrade harness builds the default and synthetic-next variants from one
tree. Synthetic-next moves every guarded surface, the component and fleet
ranges, wire ranges, cursor and journal generation, with old-format writer
pins and decoder coverage. Derived projections use regeneration instead of
lifts (`crates/lash-core-store/src/store/synthetic_next.rs:1`).

`crates/lash-upgrade-harness/tests/phase_a/main.rs:13` registers expanded-store
rollback, skipped-release refusal, writer/finalize races, wire negotiation,
object-sweep recovery, generation handover and rollback, retention and
delivery rollback, history after finalize, and workflow-graph range checks.
`operator_json_contract` lives in `crates/lashctl/tests/`.

The store matrix is SQLite file, SQLite memory and PostgreSQL. Hosts are the
in-process Restate server double, live Restate and lash-sim's in-process effect
host. Upgrade proofs use the synthetic-next tier. Phase A and rolling runs use
the two node builds against live services plus SQLite reopen cases; the
operator JSON proof needs one binary. `just phase-a` and `just e2e-rolling`
run the service proofs (`justfile:434`, `runbooks/rolling-upgrade/runbook.md:43`).
Each law's registration supplies its supported store and host combination.

`just e2e-rolling-cluster` runs the choreography under load on the Helm load
topology: three Restate nodes with replication two, PostgreSQL and kind, with
N's and N+1's workers side by side while the load driver's sessions keep
sending. It half-rolls, rolls back before finalize, rolls, finalizes with the
object sweep, and fences: the live N worker's write, a fresh N process and
N's operator are refused. The load verifier's witness classes judge no lost or
duplicated effects, stale writers fenced after finalize, the rollback
restoring N, and every session settling turns through each step
(`scripts/loadtest_upgrade.py:1`,
`runbooks/restate-postgres-workers/src/load/upgrade_verify.rs:1`). It runs on
demand and sets no performance baseline.

### 7. Release-cut guardrails

Default versions stay frozen until the 1.0 cut. Pre-1.0 shape changes keep
the current numbers. Compatibility descriptors and refusal machinery are
part of the binary; numeric schema, protocol, cursor and VM constants retain
their current default values
(`crates/lash-sqlite-store/src/schema.rs:1109`, `:1616`, `:1652`,
`crates/lash-remote-protocol/src/lib.rs:315`,
`crates/lash-sansio/src/process_cursor.rs:25`).
Synthetic-only changes do not advance those default versions.

### 8. Upgrade operations

The operator sequence is expand, roll, drain, retire, finalize, then finish
backfills, sweeps and contract. `lashctl` provides migrate, drain,
drain-status, end-drain, finalize, finalize-hold, object preflight and sweep,
preflight and version. Its exit codes are 0 done, 1 failure, 2 usage,
3 refused precondition, 4 incompatible store and 5 pending
(`crates/lashctl/src/main.rs:31`, `:107`, `:874`).

Finalize verifies the drain and live deployment registry, admits the epoch
under lock and moves it to `F_self`. PostgreSQL stores the operator hold on
the same fleet row. Automatic mode refuses a hold; `--override-hold` requests
manual mode. Rerunning a completed flip reports `already_finalized`
(`crates/lash-core-store/src/store/fleet_finalize.rs:182`,
`crates/lash-postgres-store/src/postgres/finalize.rs:105`).
SQLite finalizes the whole database set under exclusive locks and has no
fleet-wide automatic hold (`crates/lash-sqlite-store/src/backend.rs:340`).

Backfills require their declared finalized epoch. A batch advances its cursor
and rewritten rows in one guarded commit. Contract requires its epoch and
completed prerequisite backfills before it raises the reader floor
(`crates/lash-postgres-store/src/postgres/migrate.rs:267`, `:303`, `:1128`,
`:1151`, `:1516`). Object sweeps convert each family only when its newest writer format is
selected and commit each object's conversion independently (§3.2).

## Alternatives and rationale

An unlocked epoch read permits a writer to commit after finalize under an
old format. The shared row lock closes that race. Checking PostgreSQL's reader
floor on every ordinary write adds another row read: contract requires
finalize, which already fences the excluded writers. SQLite checks the floor
at every write because it shares the epoch's local row.

A second SQLite stamp or an epoch inside `_compat` adds another authority.
SQLite has one compatibility row per transaction domain, and an object
versions its own family while SQL owns fleet format selection.

One Restate wire version covers calls across all Lash handlers. A request's
drive stamp cannot replace journal routing: a stable handler can legitimately
serve a caller from another build. Mutable unknown-field preservation is also
insufficient for new semantics; writer pins must keep N's semantics before
finalize. Immutable history needs permanent decoders because no backfill can
rewrite its identity-bearing bytes.

## Consequences

PostgreSQL runtime mutations pay an epoch read and shared row lock. SQLite
mutations pay a local compatibility read. Restate values and calls carry
explicit compatibility metadata. An expanded compatible store remains
readable by the older build, while finalize fences its writers. Rollback
retains deployments needed by recorded routes. Operators use one binary with
stable DTOs and exit codes. A compatibility release carries its predecessor's
readers, writer pins and recovery operations before it uses newer formats.
