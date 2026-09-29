# 0115: The 1.0 binary carries its half of every upgrade

## Status

Accepted 2026-09-29 (FIG-3794). It pins lash 1.0's compatibility contract:
what the 1.0 binary must already contain so that it can run beside 1.1 during
a roll, and be rolled back to. Nothing below describes current behaviour
unless it cites today's code. Eleven implementation lanes build it (§9).

The frame is binding:

- 1.0 is the clean-slate release. Every stored format is reset once at the
  cut. Until then the version freeze holds (FIG-3846): shapes change in
  place, with no version bumps and no upcasters.
- After 1.0, every durable-format or wire change ships with a migration, an
  upcaster or a controlled drain.
- The 1.0 binary must already contain everything a 1.0 node needs to run
  beside 1.1 during a roll, and to be rolled back to.

Sam's earlier rulings stand: the Temporal model, automated finalize with a
hold flag, the one-release compatibility window and a typed refusal of a
skipped release ([ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md)
*Rulings*), and `lashctl` as the operator binary, with no alias binaries,
before the cut (FIG-3847, 2026-09-29).

This record amends ADR 0106:

- §2: `F` is the release compatibility epoch, and it moves at every
  compatibility release's finalize (§2 here).
- §3: one Restate wire version, carried per call, and a per-object `_compat`
  record (§3 here).
- §4: the table's rows are refined by §5 here.
- §5: the stamp carries `min_reader` on both stores (§1 here).
- §8: the order moves the writer fence, the remote-protocol negotiation, the
  reader floor, the object wire contract and `lashctl` before the cut (§8
  here).

It also corrects two facts in ADR 0106. SQLite has three versioned databases,
not four (`crates/lash-sqlite-store/src/schema.rs:41`). The deleted strict
bump gate is recoverable from the parent of `7233634ca8` (FIG-3966), not from
`932f652b45`, which is an unrelated FIG-3949 commit.

The design it ratifies is astra's study,
`/workspace/notes/lash/tasks/lanes/study-upgrade-arc.report.md`, which answers
the review `/workspace/notes/lash/tasks/lanes/review-upgrade.report.md`. Where
this record departs from the study, *Where the study is refined* says so and
why.

## Context

Every citation below was read at `8dfa6894ca`. The study was written at
`d1c9a8f2eb`. The one commit between them (FIG-4036) moved
`crates/lash-restate/src/session_driver.rs`, so the study's `:701` and
`:1162` are `:749` and `:1210` here. Every other citation of the study was
re-read and holds.

**The PostgreSQL stamp has no reader floor.** `lash_schema_versions` is
`(component, version)` (`crates/lash-postgres-store/schema.sql:17-20`). Open
admits a stamp only inside `[MIN_SUPPORTED_SCHEMA_VERSION, SCHEMA_VERSION]`
(`crates/lash-postgres-store/src/postgres/schema.rs:35-37`), and the minimum
is the latest (`crates/lash-postgres-store/src/lib.rs:633`). The shape check
reports every column it does not expect
(`crates/lash-postgres-store/src/postgres/schema_shape.rs:719-726`). So after
1.1's expand, a restarting 1.0 pod refuses the store as a newer build's.

**SQLite's stamp is `PRAGMA user_version` alone.** The versioned open compares
it for equality (`crates/lash-sqlite-store/src/schema.rs:1619-1646`), and a
single integer cannot carry a floor. The fleet-format row and the release
stamp live only in the durable core
(`crates/lash-sqlite-store/src/schema.rs:1655-1668`). The process registry
and trigger databases are separate files with their own transactions.

**`F` is read once, at open.** PostgreSQL reads it inside the open
transaction (`crates/lash-postgres-store/src/postgres/schema.rs:124`), the
handle keeps it (`crates/lash-postgres-store/src/postgres/fleet_format.rs:146`),
and SQLite does the same (`crates/lash-sqlite-store/src/fleet_format.rs:145-147`).
A finalize cannot reach a writer that is already open. The session commit
encodes its payloads under that `F` before `BEGIN`
(`crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:281-287`).
PostgreSQL begins transactions at 112 sites in 31 files, and runs 73 more
statements, reads and writes, straight on the pool. SQLite's write
transactions go through `SqliteConnection::write` and `write_flow`, both
`BEGIN IMMEDIATE`
(`crates/lash-sqlite-store/src/conn.rs:389-417`, `:423-437`).

**The read window forgets history.** A reader admits exactly the build's
newest version or `F`'s recorded one
(`crates/lash-core-store/src/store/fleet_format.rs:186-188`). After a
finalize, N-1 history would be refused.

**The remote protocol is exact-match.** Decode refuses any version other than
`REMOTE_PROTOCOL_VERSION`, which is 100
(`crates/lash-remote-protocol/src/lib.rs:313`, `:446-451`). The refusal
carries `{actual, expected}`, so a newer peer cannot learn a range.

**Restate calls cross builds without a version.** Restate pins an invocation
to the deployment it started on, sends each new invocation to the newest
deployment that serves its name, and shares object state across deployments.
`EFFECT_GROUP_WIRE_VERSION` is not transmitted: no request field carries it
(`crates/lash-restate/src/effect_group/protocol.rs:18-34`). Object values are
always written under the build's current format, whatever `F` says
(`crates/lash-restate/src/object_state.rs:201-217`). `LashTurn` keeps its
`RootOutcome` as a bare value (`crates/lash-restate/src/session_driver.rs:930`,
`:1210`), and a pinned drive of one build reads it through the stable lane,
which the newest build serves (`:1035-1068`).

**Two findings the study does not name.**

- *A pinned drive's root dies when the drive version moves.* Both session
  handlers refuse a request whose `drive_version` differs from their own
  (`crates/lash-restate/src/session_driver.rs:880`, `:906`). A drive pinned
  to N sends its admitted root to the stable `LashTurn`, which N+1 serves. If
  N+1 bumped `LASH_SESSION_DRIVE_VERSION`, the call is refused, the drive
  attaches to an outcome that was never recorded, and the root is `Released`
  (`:1057-1080`). The lost-run pass then ends it `SubstrateLost`. The request
  crosses builds, so its stamp must not act as a drain gate.
- *Registration overwrites any deployment at the same URI.* The admin call
  always sends `force: true` (`crates/lash-restate/src/ingress.rs:1216-1219`).
  The name guard lets through a deployment "held by a deployment at the same
  `uri`" ([ADR 0111](0111-a-deployment-namespace-prefixes-every-restate-name.md)
  §4). A host that rolls pods behind one stable URI would re-register N+1
  over N's deployment, and N's pinned journals would then reach N+1's code.

**Surfaces with no version.** Module artifacts have no envelope, and `verify`
recomputes the identity with today's hashing
(`crates/lashlang/src/artifact.rs:331-341`, `:457-460`). SQLite's blob
envelope `{compression, content}` is unversioned and unregistered
(`crates/lash-sqlite-store/src/lib.rs:645-649`,
`crates/lash-sqlite-store/src/codec.rs:84-111`). An unknown obligation state
is `StoreError::Backend` (`crates/lash-core-store/src/store/obligation.rs:386-391`).
An unknown attachment owner kind is `StoredDataCorrupt`
(`crates/lash-core-store/src/store/attachment_manifest.rs:145-150`).
`lash-migrate drain-status --json` prints an internal status type
(`crates/lash-postgres-store/src/bin/lash_migrate.rs:287-288`).

**The registry.** `scripts/versioned-surfaces.toml` registers 71 surfaces and
21 unregistered constants. One exclusion is stale:
`QUEUED_WORK_CLAIM_LEASE_ENCODING_VERSION` names a constant that FIG-3946
deleted.

## Decision

Four numbers stay distinct, as ADR 0106 has them:

| Number | What it versions | Where it lives |
|---|---|---|
| A component version and its `min_reader` | one stored schema (PostgreSQL, each SQLite database) or one Restate object family | the store's stamp row; the object's `_compat` record |
| `F`, the fleet epoch | which version of every format the fleet writes | `lash_fleet_format` (PostgreSQL); each SQLite database's `lash_compat` row |
| A wire version | a message shape two builds exchange | negotiated per connection (remote protocol) or per call (Restate) |
| `G`, the drain generation | the meaning of a journal | the deployment's generation lane and the records that route to it |

One law covers every surface that more than one build reads:

> **Before finalize, N+1 writes only what N reads.** Every stored value, row,
> object value, cursor and request that N+1 writes while `F` is N's epoch is
> in N's shape and has N's semantics. That includes retention and delivery
> semantics: no new referrer or obligation kinds, no new identity families,
> no new required behaviour, and no new column that N would lose on
> rewrite. N+1 stamping N's number on a new shape breaks the law.

So the 1.0 binary never needs to read a 1.1 shape before finalize, and
after finalize no 1.0 code runs. What 1.0 must carry is the machinery that
lets 1.1 keep that law: stamps it honours, a fence it obeys, ranges it
declares and refusals that 1.1 can read.

### 1. The component compatibility descriptor

#### 1.1 Types

```rust
// crates/lash-sansio/src/compat.rs (new)
/// A non-empty inclusive range of versions of one surface. JSON
/// `{"min":1,"max":1}`, frozen: every build parses every range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(try_from = "RawVersionRange", into = "RawVersionRange")]
pub struct VersionRange { min: u32, max: u32 }

impl VersionRange {
    pub const fn exactly(version: u32) -> Self;
    /// Refuses `min == 0` and `min > max`.
    pub fn new(min: u32, max: u32) -> Result<Self, VersionRangeError>;
    pub const fn min(self) -> u32;
    pub const fn max(self) -> u32;
    pub const fn contains(self, version: u32) -> bool;
    /// The highest version both ranges contain; `None` when disjoint.
    pub fn select(self, peer: Self) -> Option<u32>;
}
```

```rust
// crates/lash-core-store/src/compat.rs (new)
/// One versioned stored component: a PostgreSQL schema, one SQLite
/// database, or one Restate object family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ComponentId(&'static str);

/// What this build declares about one component.
#[derive(Clone, Copy, Debug)]
pub struct CompatDescriptor {
    pub component: ComponentId,
    /// The stamps this build opens: `[oldest it still reads, newest it knows]`.
    pub reads: VersionRange,
    /// The versions its migrations or encoders can produce.
    pub writes: VersionRange,
}

/// A durable stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompatStamp { pub version: u32, pub min_reader: u32 }

pub enum CompatAdmission {
    /// The stamp is inside `reads`.
    Native,
    /// A newer release expanded the component, and its floor still admits
    /// this build. The shape check runs in tolerant mode (§1.4).
    Expanded { version: u32 },
}

/// The admission rule of §1.3.
pub fn admit(
    descriptor: &CompatDescriptor,
    stamp: StampRead,
) -> Result<CompatAdmission, CompatRefusal>;

pub enum StampRead { Absent { populated: bool }, Present(CompatStamp), Unreadable(String) }
```

`lash_core_store::compat::DESCRIPTORS` lists every component this build
declares, and `lashctl version --json` prints them beside `G`, `F`'s writable
range and every wire range (§4, §5).

The components at 1.0:

| `ComponentId` | Stamp lives in | `reads` / `writes` at the cut |
|---|---|---|
| `postgres` | `lash_schema_versions` row `lash-postgres-store` | `[1,1]` / `[1,1]` |
| `sqlite-core` | durable-core database, `lash_compat` | `[1,1]` / `[1,1]` |
| `sqlite-registry` | process-registry database, `lash_compat` | `[1,1]` / `[1,1]` |
| `sqlite-triggers` | trigger database, `lash_compat` | `[1,1]` / `[1,1]` |
| `restate-effect-group-state` | each `EffectGroupIndex` object's `_compat` | `[1,1]` / `[1,1]` |
| `restate-effect-group-payload` | each `EffectGroupPayload` object's `_compat` | `[1,1]` / `[1,1]` |
| `restate-durable-wait-registry` | each `LashDurableWaitIndex` object's `_compat` | `[1,1]` / `[1,1]` |

#### 1.2 Where the stamps live

**PostgreSQL.** The existing table gains the floor:

```sql
CREATE TABLE IF NOT EXISTS lash_schema_versions (
    component TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    min_reader INTEGER NOT NULL,
    CONSTRAINT ck_lash_schema_versions_stamp
        CHECK (version >= 1 AND min_reader >= 1 AND min_reader <= version)
);
```

`F` stays in `lash_fleet_format` (`crates/lash-postgres-store/schema.sql:49-53`).
`lash_release_stamp` keeps naming the release that last wrote the database;
it is evidence for operators, never an admission input.

**SQLite.** Every one of the three databases gets the same row:

```sql
CREATE TABLE IF NOT EXISTS lash_compat (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    component TEXT NOT NULL,
    version INTEGER NOT NULL,
    min_reader INTEGER NOT NULL,
    fleet_format INTEGER NOT NULL,
    CHECK (version >= 1 AND min_reader >= 1 AND min_reader <= version
           AND fleet_format >= 1)
);
```

`F` is replicated into each database because each database is its own
transaction domain, and the fence (§2) must read it inside the writer's own
transaction. The durable core's `fleet_format` table is deleted, and so is
the rule that `PRAGMA user_version` is the stamp. Open never reads
`user_version` again. The row is the only authority.

**Restate.** §3.2 gives each object a `_compat` record.

#### 1.3 The admission rule

`admit` answers in this order:

1. **Absent.** An empty store (no lash objects) is provisioned by the
   component's installer, which writes the stamp. A populated store without
   a stamp is `Unstamped`. A stamp is never defaulted to the current version.
2. **Unreadable or malformed.** A stamp that does not decode, or whose
   `min_reader` is 0 or above its `version`, is `MalformedStamp`.
3. **Too old.** `version < reads.min` is `TooOld`: an older or skipped
   release wrote it. Stepping through the intermediate release is the remedy
   (ADR 0106 Q3).
4. **Floor passed.** `min_reader > reads.max` is `ReaderFloorAbove`: a newer
   release contracted past this build.
5. **Admitted.** Otherwise `Native` when `version <= reads.max`, else
   `Expanded`.

Every store open runs it before it takes traffic. `F` is admitted separately
(§2.1), and a skipped compatibility release is refused there too.

Expand keeps `min_reader`. Only a contract step raises it, and only to a
component version whose release has been finalized. So N admits every store
N+1 expanded, and refuses one that N+2 contracted.

#### 1.4 The shape check tolerates safe additions

`Native` runs today's exact shape check. `Expanded` runs a tolerant one:

- **Required:** every object this build expects is present and satisfies
  its expected definition, exactly as today.
- **Tolerated:** a table this build does not name; a view; a non-unique
  index; a column that is nullable or has a default and carries no
  constraint beyond its type.
- **Refused as `ShapeRefused`:** on any table this build expects, a NOT NULL
  column without a default, a CHECK, UNIQUE, FOREIGN KEY or EXCLUDE
  constraint, or a trigger this build does not expect. A `NOT VALID`
  constraint counts: PostgreSQL still enforces it on new rows, so it would
  reject N's writes.

On PostgreSQL this is a classification step in
`crates/lash-postgres-store/src/postgres/schema_shape.rs` over the findings
the comparison already produces. SQLite gains the same check for `Expanded`
databases, read from `sqlite_schema`, `pragma_table_info` and
`pragma_index_list`. `Native` SQLite databases keep today's no-introspection
fast path.

The migration catalog enforces the other side. An expand step may add only
tolerated objects. A unit test in `migrate.rs` applies each expand step to
the previous component's catalog and runs the previous component's tolerant
check on the result.

#### 1.5 Typed refusals

`StoreError` changes in place. `SchemaVersionOutOfRange` and
`FleetFormatOutsideWritableRange` fold into one variant, and the fence adds
one:

```rust
// crates/lash-core-store/src/store/error.rs
Incompatible { refusal: CompatRefusal },
WriterFenced { recorded: u32, writable: VersionRange },

// crates/lash-core-store/src/compat.rs
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(tag = "refusal", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompatRefusal {
    Unstamped { component: String },
    MalformedStamp { component: String, detail: String },
    TooOld { component: String, found: u32, reads: VersionRange },
    ReaderFloorAbove { component: String, found: u32, min_reader: u32, reads: VersionRange },
    ShapeRefused { component: String, findings: Vec<String> },
    /// `F` at open: below the writable range (a skipped release) or above it
    /// (a newer fleet).
    FleetOutsideWritable { recorded: u32, writable: VersionRange },
    /// The SQLite databases of one store disagree on their stamps or `F`.
    PartiallyAdvanced { databases: Vec<(String, CompatStamp, u32)> },
    /// A stored label this build has no name for (an obligation state or
    /// kind, an attachment owner kind, a referrer kind): a newer build wrote
    /// it. Classified apart from corruption so no pass treats it as absent.
    UnknownVocabulary { surface: String, label: String },
}
```

Each refusal's message names its remedy with a `lashctl` command. The
FIG-3796 text that names `lash admin finalize-upgrade`
(`crates/lash-core-store/src/store/error.rs:148`) goes.

### 2. The fleet epoch `F` and the writer fence

#### 2.1 `F` is the release compatibility epoch

`F` is 1 at the cut. Every compatibility release declares a writable range
`[F_prev, F_self]`, and its finalize moves `F` to `F_self`. It moves even
when no format changed, because moving it is what fences the old release's
writers. `FleetFormat::writer_version(surface)` maps `(F, surface)` to the
version the fleet writes, through the build's `WRITER_PINS`
(`crates/lash-core-store/src/store/fleet_format.rs:268`). An open admits
`F` against the writable range. Below it is a skipped release, and above it
is a newer fleet; both are `FleetOutsideWritable`.

#### 2.2 The guarded transaction entry

Every mutation checks `F` inside its own transaction. That covers session
commits, admission, turn inputs and queued work, relay claims and
settlements, the leader lease, process registry and trigger writes,
attachments, artifact publication and cleanup, retention, GC, deletes, the
definition registry, drain marks and the migrations ledger. Reads are not
fenced: a stale reader meets stamps it refuses, typed.

**PostgreSQL.** A new `crates/lash-postgres-store/src/postgres/guarded_tx.rs`
is the only place a mutating transaction begins:

```rust
pub(crate) struct GuardedTx<'c> { /* the transaction and the F it read */ }

impl GuardedTx<'_> {
    /// The epoch this transaction runs under.
    pub(crate) fn fleet(&self) -> FleetFormat;
    /// Refuses with `FleetMoved` when payloads encoded before BEGIN were
    /// encoded under another epoch.
    pub(crate) fn require_encoded_under(&self, fleet: FleetFormat) -> Result<(), StoreError>;
}

/// BEGIN, then the fence as the transaction's first statement.
pub(crate) async fn begin_guarded(pool: &PgPool, fence: &WriterFence) -> Result<GuardedTx<'static>, StoreError>;

/// `f` in a guarded transaction, retried per §2.4.
pub(crate) async fn guarded<T, F>(pool: &PgPool, fence: &WriterFence, f: F) -> Result<T, StoreError>;
```

The fence is one indexed statement, and it runs before anything else,
including the session advisory lock and every row lock:

```sql
SELECT format_version FROM lash_fleet_format WHERE singleton FOR SHARE;
```

Finalize reads the same row `FOR UPDATE` and then updates it, in one
transaction that takes no other row lock. The ordering follows from
PostgreSQL's row locks:

- A writer that holds the row `FOR SHARE` makes finalize wait. The writer
  commits first under the old `F`, which is still correct.
- A writer whose `FOR SHARE` waits behind finalize re-reads the row once
  finalize commits. Under `READ COMMITTED` it sees the new `F`. Under
  `REPEATABLE READ` it fails `40001` and is retried. Either way it is fenced
  before it writes anything.
- A writer never holds another lock while it waits on `F`, and finalize
  takes no lock a writer holds. So the two cannot deadlock.

The cost is one round trip and one shared row lock per mutating
transaction. Writers stay concurrent with each other. An unlocked `SELECT`
would not do: a writer could read the old `F`, finalize could commit, and
the writer could then commit rows under a retired epoch. A row lock needs
`UPDATE` privilege, so a writing role needs it on `lash_fleet_format`, and
the host-provisioned grant list (`runbooks/host-provisioned-schema/`) gains
it. A role with only `SELECT` never mutates, so it never runs the fence.

**SQLite.** The fence runs inside `SqliteConnection::write` and `write_flow`
(`crates/lash-sqlite-store/src/conn.rs:389`, `:423`), as the first statement
after `BEGIN IMMEDIATE`:

```sql
SELECT component, version, min_reader, fleet_format FROM lash_compat WHERE singleton = 1;
```

It is a local read with no network round trip. `BEGIN IMMEDIATE` holds the
database's reserved lock, so no other writer (and no finalize) can change the
row until this transaction ends. SQLite also re-admits the component stamp
here (§1.3), because another process can migrate a shared database on open
while this one holds a connection. That costs nothing extra: it is the same
row.

A SQLite migration or finalize owns the whole store. It takes
`BEGIN EXCLUSIVE` on every database in `SqliteDatabase::ALL` order, rewrites
each `lash_compat` row, and commits in the same order. A crash between those
commits leaves the databases disagreeing. On reopen, a build whose
migrations cover the gap completes the set forward. Any other build refuses
it with `PartiallyAdvanced`.

#### 2.3 Pre-encoded commits

A commit's payloads are encoded before `BEGIN` under the handle's last
observed `F`, which is the value the most recent fence read. Inside the
transaction, the guard compares the `F` it just read with the encoding
epoch:

- Equal: proceed.
- Different and writable: roll back with the internal `FleetMoved`. The
  store encodes again under the new `F` and retries once.
- Not writable: `WriterFenced`.

`F` moves at most once per release, so this costs nothing until 1.1's
finalize and adds one retry to at most one transaction per writer after it.
`FleetMoved` never escapes a store call.

The handle's `FleetFormatStore::fleet_format()` answers the last `F` the
fence observed, not the open-time value. The Restate object encoders read it
too (§3.3).

#### 2.4 Retry classes

| Failure | Error | Action |
|---|---|---|
| `40001`, `40P01`, `55P03`; `SQLITE_BUSY`, `SQLITE_LOCKED` | `StoreError::Contended` (today's class, `crates/lash-postgres-store/src/postgres/support.rs:267-278`) | retry the whole transaction, fence included |
| `F` moved to a writable epoch | `FleetMoved` (internal) | encode again under the new `F`, retry once |
| `F` outside the writable range | `StoreError::WriterFenced` | terminal, no retry; the deployment reports itself fenced and takes no more work |
| fence row missing or malformed | `StoreError::Incompatible` | terminal; fail closed |
| connection or I/O | `StorageFailure` | as today |

A fenced transaction wrote nothing, because the fence is its first
statement and the rollback takes the rest.

A lint (`scripts/check-guarded-transactions.py`) keeps the entry total. It
fails on any `.begin()`, `pool.begin()` or mutating statement run straight on
a pool under `crates/lash-postgres-store/src/`, and on any rusqlite write
transaction under `crates/lash-sqlite-store/src/`, outside the guard and an
allowlist of read-only sites (`scripts/guarded-transaction-readonly.txt`).
Each allowlist entry says why it only reads.

### 3. Restate

SQL fencing cannot fence Restate atomically: no SQL transaction spans a
Restate call. Restate state is instead protected by four rules:

- the law above (N+1 writes N's shape until finalize);
- per-call wire ranges;
- a per-object floor;
- finalize's precondition that the old generation is drained **and** its
  deployment removed (ADR 0106 §2).

#### 3.1 Every cross-build call carries the caller's range

One wire version, `RESTATE_WIRE_VERSION`, covers every Lash handler that a
build other than the caller's can serve. It replaces
`EFFECT_GROUP_WIRE_VERSION`. That covers:

- every handler of `EffectGroupIndex`, `EffectGroupPayload` and
  `LashDurableWaitIndex`;
- the shared handlers of `LashDurableWaitWorkflow`, `LashProcessWorkflow`
  and `EffectGroupDispatch`;
- `LashSession.drive`, `LashTurn.run` and `LashTurn.outcome`;
- `LashProcessAttach.run`.

Generation-lane handlers are served only by their own `G`, but they use the
same envelope so that one handler body serves both lanes.

```rust
// crates/lash-restate/src/compat.rs (new)
pub const RESTATE_WIRE_VERSION: u32 = 1;
pub const RESTATE_WIRE: VersionRange = VersionRange::exactly(RESTATE_WIRE_VERSION);

/// Every cross-build request. JSON `{"wire":{"min":1,"max":1},"body":…}`;
/// the outer shape is frozen.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Call<T> { pub wire: VersionRange, pub body: T }

/// Every cross-build reply. JSON `{"wire":1,"body":…}`; frozen.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Reply<T> { pub wire: u32, pub body: T }
```

- **Requests** are encoded at the caller's `F`-selected wire version, so any
  build in the window can read them. `wire` states every version the caller
  reads.
- **The handler** selects `RESTATE_WIRE.select(call.wire)` before it reads
  or writes any state. A disjoint range is a terminal
  `lash.wire_unsupported` error that carries both ranges, with nothing
  changed.
- **Replies** use the selected version. So an old pinned caller gets an old
  reply from a new handler, and a new caller gets a reply it reads from an
  old handler after a rollback.
- **A request's stamp is not a drain gate.** `drive_version` leaves
  `RestateSessionDriveRequest` and `RestateTurnDriveRequest`
  (`crates/lash-restate/src/session_driver.rs:135-153`). Its check at `:880`
  and `:906` is deleted. The journal a stable-lane call starts is the serving
  build's, and the generation sentinel already guards replay.
  `LASH_SESSION_DRIVE_VERSION` stays a D surface, as an input to `G`.

#### 3.2 The per-object `_compat` record

Every Lash object carries one record under the key `_compat`:

```json
{"format": 1, "min_reader": 1, "min_writer": 1}
```

Its shape is frozen and never enveloped. `format` is the oldest family
format any value in the object may carry. `min_reader` and `min_writer` are
the oldest family formats a build must support to read, or to mutate, the
object.

- **Every handler reads `_compat` first**, after the wire selection and
  before any other state. A shared handler requires
  `min_reader <= reads.max`. An exclusive handler also requires
  `min_writer <= writes.max`. A failure is a terminal `lash.incompatible`
  error carrying the `CompatRefusal`, with nothing changed.
- **Absent `_compat`.** On an object with no other keys, the first exclusive
  handler writes it at the selected format. On a populated object it is
  `Unstamped`. At the cut every object is fresh, so this only catches state
  from before the cut.
- **Clear and retire keep it.** `retire`, `finish_retirement`, `cancel_all`,
  `revoke_all` and the payload's `delete_bytes` check `_compat` like every
  other exclusive handler, and they never delete it. A retired object's
  `_compat` is what fences a stale handler from recreating its state.
- **Only the next release raises it.** Its `upgrade` handler (FIG-4041,
  post-1.0) rewrites the object's values, then raises all three fields in
  the same exclusive invocation. After finalize this fences any leftover N
  handler, even one an operator kept by force-removing a deployment.

Introspection (Restate SQL over state) measures sweep progress by `format`.
It never fences.

#### 3.3 The selected encoder

`set_stamped` stops writing `formats.current`
(`crates/lash-restate/src/object_state.rs:213`). It takes the writer the
fleet selects:

```rust
pub(crate) fn set_stamped<T: Serialize + 'static>(
    ctx: &ObjectContext<'_>, key: &str, writer: StoredValueWriter, body: T,
) -> Result<(), TerminalError>;

impl StoredValueFormats {
    /// `fleet.writer_version(surface)`, with the family's down-converters
    /// (none at 1.0).
    pub(crate) fn writer(&'static self, fleet: FleetFormat) -> StoredValueWriter;
}
```

`fleet` is the deployment store's `fleet_format()` (§2.3). A stale view is
safe in one direction only, and that is the direction it can be stale in.
Before finalize every build sees N's epoch. After finalize, a build that has
not yet observed the move writes N's format, which N+1 reads. The
`{format, body}` envelope (`object_state.rs:29-35`) is kept as it is.

#### 3.4 The versioned `RootOutcome`

`LashTurn`'s `outcome` state is stored as `{format, body}` under a new
registered format, `LASH_TURN_OUTCOME_FORMAT_VERSION` (M). Its readers
dispatch on the stamp, like the object families. The `run` reply, the
`outcome` reply and `LashSession.drive`'s `DriveOutcome` travel in `Reply`
at the selected wire version. Pinning the run protects only the run itself;
the stamp and the wire protect the readers that come later.

#### 3.5 Deployment and rollback routing

1. **Immutable endpoints.** Each build registers at a URI that no deployment
   of another `G` holds. `register_deployment` stops forcing blindly. It
   reads the deployment at the URI and then acts on what it finds:
   - none: register without force;
   - one that serves this build's generation names (`…_g<G>`): register with
     force, because this is a redeploy of the same build;
   - any other: refuse with
     `RestateRegistrationError::EndpointServesAnotherGeneration { uri, held, local }`.
2. **One namespace across the roll.** N and N+1 bind under the same ADR 0111
   namespace. Moving namespace is a new deployment, not an upgrade.
3. **Stable names.** Object and state-holding service names never split by
   `G` (ADR 0106 §1). Their state is shared, and §3.2 and §3.3 protect it.
4. **Recorded routes are data.** A redrive, a group child or a refused
   successor goes to the generation the record names, never to one
   recomputed from the caller's `G`. The generation sentinel
   (`crates/lash-restate/src/process/workflow.rs:1031`), unreadable-input
   routing (`crates/lash-restate/src/process/mod.rs:1261`) and the
   successor-window check (`crates/lash-restate/src/process/workflow/lanes.rs:93`)
   stay as they are.
5. **A journal never moves to another build.** A continuation, parked turn
   or segment state outside the serving build's read range parks and routes
   to its writer's `G`. The VM fence
   (`crates/lashlang/src/runtime/vm/continuation.rs:1511`) is never relaxed.
6. **Rollback.** Register N's build at a fresh URI, which makes it the
   newest deployment for new invocations. N+1's deployment stays registered,
   never overwritten and never removed, until its own pinned invocations
   drain. `lashctl drain <G_N+1>` runs the drain in reverse. N serves
   everything N+1 wrote, because of the law. A continuation N+1 parked that N
   cannot decode routes to `G_N+1`, which N+1's deployment still serves.
7. **Retirement is by deployment, not heartbeat.** Finalize requires that no
   registered deployment serves `G_N`'s generation names, and that
   `drain_status(G_N)` reads drained. The drain counts every invocation
   pinned to a deployment of that generation, whatever its handler kind.
   Inboxed object calls are not pinned yet; they start on the newest
   deployment.

### 4. Remote protocol negotiation

`crates/lash-remote-protocol/src/negotiation.rs` (new) holds the bootstrap.
Its three messages carry no `protocol_version`. They precede selection, and
their JSON is frozen forever, so every build parses every peer's.

```rust
pub const REMOTE_PROTOCOL: VersionRange; // [1,1] at the cut

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "negotiation", rename_all = "snake_case")]
pub enum Negotiation {
    Hello { supported: VersionRange },
    Accept { supported: VersionRange, selected: u32 },
    Unsupported { local: VersionRange, peer: VersionRange },
}

/// A server's answer to a `Hello`.
pub fn answer(local: VersionRange, hello: &Negotiation) -> Negotiation;

/// A connection's selected version. Built only from an `Accept` that this
/// side validated: `selected` is inside both ranges.
pub struct Negotiated { selected: u32 }
impl Negotiated {
    pub fn from_accept(local: VersionRange, accept: &Negotiation) -> Result<Self, RemoteProtocolError>;
    pub fn selected(&self) -> u32;
}
```

- **Selection.** The highest version in both ranges. A 1.1 peer offering
  `[1,2]` and a 1.0 peer offering `[1,1]` select 1.
- **No intersection.** The answer is `Unsupported`, and nothing runs.
- **Every message names its version.** `Envelope::new` (always current) is
  deleted. `Envelope::at(&Negotiated, body)` encodes a request.
  `Envelope::decode_json(bytes, local)` accepts any `protocol_version`
  inside `local`, and it checks the version before it decodes the body. A
  responder answers with `Envelope::reply_to(&request, body)`. Responses,
  errors and stream events use the request's version.
- **Out of range.** A version outside `local` is
  `RemoteProtocolError::Unsupported { local, peer }`, replacing
  `UnsupportedProtocolVersion { actual, expected }`
  (`crates/lash-remote-protocol/src/registry_errors.rs:63`). It is refused
  before any decode or effect.
- **Load-balanced peers.** Each connection negotiates, and every request
  carries its version. A peer behind a load balancer that did not see the
  `Hello` validates each request by itself.
- **Encoders per version.** A DTO is encoded at the selected version. 1.0
  has only version 1. 1.1 adds version 2 and keeps the version-1 encoder as
  a down-conversion.

Hosts own their transports, and lash owns the messages and the rule.
`REMOTE_PROTOCOL_VERSION` resets from 100 to 1 at the cut.

Shared Restate calls use the same `VersionRange` and the same selection rule
(§3.1), carried per call rather than per connection.

### 5. Per-surface obligations

The obligations are:

- **tolerate:** read an older or expanded shape;
- **preserve:** keep bytes and identities exactly as stored;
- **refuse typed:** name the version and the remedy, keep the bytes, and
  change nothing;
- **route:** send the work to the build that wrote it.

| Surface | Registered as | 1.0 obligation |
|---|---|---|
| PostgreSQL schema | `SCHEMA_VERSION` (pg), M | Admit per §1.3. Tolerate safe additions (§1.4). Refuse typed. |
| SQLite databases | the three `*SCHEMA_VERSION`, M | Admit per §1.3 at open **and** in every write transaction. Refuse a partially advanced set. |
| `F` | `FLEET_FORMAT_VERSION`, M | Admit at open, fence every mutation (§2), and write only `F`-selected versions. |
| Mutable rows (`SESSION_HEAD_META`, `PROTOCOL_TURN_OPTIONS`, `SCOPE_STORAGE_PAYLOAD`, `PROCESS_WAKE_DELIVERY_FORMAT`, `NATIVE_DRIVER_STATE`, `CURRENT_SESSION_STATE`) | M | Write at `F`. Refuse a newer stamp typed. N+1 never emits a field N would drop on rewrite before finalize (the law), so 1.0 carries no unknown-field preservation. |
| Session checkpoints and immutable history (`SESSION_NODE_BODY`, `SESSION_CHECKPOINT`, `CHECKPOINT_COMPONENT_ENCODING`, `RUNTIME_COMMIT_RECEIPT`, `RLM_SNAPSHOT`, `LASHLANG_SNAPSHOT`, `HEAP_SIZE_SCHEDULE`, `NATIVE_TRANSPORT`, `PROCESS_EVENT_VOCABULARY`) | M, read-only | Preserve bytes and hashes, and never rewrite. Refuse an unsupported encoding typed. The read window gains a permanent history floor that `F` does not move: `ReadWindow` admits `[history_floor, newest]` for these surfaces instead of `{recorded, newest}` (`fleet_format.rs:186`). At 1.0 the floor is 1, and FIG-3802 fills in the upcasters. |
| VM continuations, parked turns, segment state (`VM_CONTINUATION_FORMAT`, `TURN_CHECKPOINT_SCHEMA`, `LASHLANG_SEGMENT_STATE`) | D | Decode only what this build supports. Otherwise park and route to the recorded `G` (§3.5). N+1 supplies the conversion at the segment boundary. A full rollback never kills N+1's remaining pinned work. |
| Semantic hashes and identity families (`LASHLANG_SEMANTIC_HASH`, `BYTECODE_FORMAT`, `*_FAMILY_VERSION`, `FRAME_KEY`, `JOURNAL_IDENTITY`, the four request-identity encodings) | C | Verify a stored identity under the family it names. Never recompute it under new rules. A new family is minted only after finalize. |
| Module artifacts | new: `MODULE_ARTIFACT_ENVELOPE_VERSION`, C | Stored artifacts gain an envelope `{"family": …, "encoding": …, "artifact": …}`. `verify` checks the stored `module_ref` under the family the envelope names, never under today's hashing. An unknown family or encoding is refused typed (`ModuleArtifactError::UnsupportedFamily`), not `HashMismatch`. |
| SQLite blob envelope | new: `SQLITE_BLOB_ENVELOPE_VERSION`, M | `StoredBlobEnvelope` gains `version`. An unknown version or compression is refused typed and the bytes are kept. The old compression codecs stay while `F` is N. PostgreSQL and S3 store raw bytes and are unchanged. |
| Attachments | (store rows) | Preserve bytes and digest equality. An unknown owner kind is `Incompatible(UnknownVocabulary)`, not `StoredDataCorrupt`. GC treats an owner it cannot decode as referenced: it never deletes because it failed to read. |
| ADR 0113 referrers and fences | new: `ARTIFACT_REFERRER_KINDS_VERSION`, C | The seven kinds and their canonical ids are the 1.0 baseline. An unknown kind is `Incompatible(UnknownVocabulary)`; this amends ADR 0113 §1, which maps every decode refusal to `StoredDataCorrupt`, for that one case. The cleanup executor stops its pass at an edge or fence it cannot decode, with the obligation stalled `undecodable`; it never counts that edge as absent. No new kind before finalize. |
| Obligations and outboxes (ADR 0109) | new: `OBLIGATION_LEDGER_VOCABULARY_VERSION`, M | Old states, kinds, payloads and idempotency keys stay while `F` is N. An unknown state or kind label is `Incompatible(UnknownVocabulary)`, not today's `Backend` (`obligation.rs:386`). The relay stalls a row whose kind it cannot decode `undecodable`, as ADR 0109 §1.3 already does for a key that does not decode. The row stays outstanding and visible, and it is never acknowledged or discarded. A state label no build of this window wrote is never selected by the due read, so it is left untouched. |
| Restate object state (three families) | M | §3.2 and §3.3. |
| `LashTurn` outcome | new: `LASH_TURN_OUTCOME_FORMAT_VERSION`, M | §3.4. |
| Restate journals and inputs (`EFFECT_JOURNAL`, `RESTATE_PROCESS_JOURNAL`, `LASH_SESSION_DRIVE`, `JOURNAL_LOGIC_EPOCH`, and the other D rows) | D | Route to the writer. The sentinel parks a foreign journal before any effect. |
| Restate handler wire | `RESTATE_WIRE_VERSION`, C | §3.1. |
| Remote protocol | `REMOTE_PROTOCOL_VERSION`, C | §4. |
| Trace JSONL | `TRACE_SCHEMA_VERSION`, C | Every record carries its version. A sink writes its own build's version; trace is per process and no lash code reads it back. Readers accept every version they know, ignore unknown optional fields and skip unknown event kinds, counting them: trace is observational and carries no executable variant. |
| Process cursors | `PROCESS_CURSOR_VERSION`, C (registered at the cut) | Minted at the `F`-selected version, so a cursor survives rollback. A cursor outside the read range is refused typed, and the host contract is to list again from a fresh cursor. |
| `lashctl --json` | new: `LASHCTL_JSON_SCHEMA_VERSION`, C | Every command prints `{"schema_version":1,"command":…,"result":…,"error":…}` and exits with a pinned code: 0 done, 1 unexpected failure, 2 usage, 3 refused precondition, 4 incompatible store, 5 not yet (a drain still pending, or a wait timed out). `result` shapes are DTOs owned by `lashctl`, never internal status types. |
| Derived projections (`WORKFLOW_GRAPH_SCHEMA`, `WORKFLOW_TYPE_FACET`) | M | Regenerate from the module. Refuse a newer stamp typed. |
| `PRODUCT_EVENT_LOG_FORMAT` | M | Owned by the agent-workbench example. It is not part of lash's contract. |

**Unknown fields.** No blanket tolerance applies:

- Observational records (trace, stream events, preflight and status
  reports) ignore unknown optional fields.
- Records that carry effects, ownership or identity admit only what they
  can type, and refuse the rest.
- Mutable records need no preservation, because the law forbids emitting
  what N cannot carry until N is retired.

### 6. Phase A: the synthetic N+1 gates

Phase A (FIG-3805 A) is a release gate for the cut. Head is built twice:

- **N** is the default build.
- **N+1** is the same tree with the `synthetic-next` Cargo feature. The
  feature bumps the PostgreSQL component (one expand step adds a nullable
  column, a table and a non-unique index), every SQLite component, `F`'s
  writable range to `[1,2]` with one writer pin, the remote protocol to
  `[1,2]` with one added field, `RESTATE_WIRE_VERSION` to 2,
  `EFFECT_GROUP_STATE_FORMAT_VERSION` to 2 (with its N-1 upcaster and a
  synthetic `upgrade` handler), `SESSION_NODE_BODY_SCHEMA_VERSION` (with its
  permanent history upcaster), `VM_CONTINUATION_FORMAT_VERSION`, and
  `JOURNAL_LOGIC_EPOCH`, so `G` changes.

The feature lives only as `#[cfg(feature = "synthetic-next")]` blocks beside
the constants it moves. The facade forwards it. No production build enables
it.

Both builds are the `lash-upgrade-node` binary of the new crate
`crates/lash-upgrade-harness`. Its tests run the two binaries as separate
processes against real PostgreSQL and a real multi-node Restate, plus SQLite
reopen cases. They live in `crates/lash-upgrade-harness/tests/phase_a/`:

| Test | What it proves |
|---|---|
| `expanded_store_rollback` | N+1's migrate expands PostgreSQL and every SQLite database. N restarts, opens `Expanded`, writes, and N+1 reads N's rows. Raising `min_reader` makes N refuse `ReaderFloorAbove` on both backends. Each unsafe addition of §1.4 makes N refuse `ShapeRefused`. A populated store with its stamp deleted refuses `Unstamped`. |
| `skipped_compatibility_release_refused` | A build whose writable range starts above the recorded `F` refuses at open with `FleetOutsideWritable`, before it takes traffic. So does one whose component range starts above the stamp. |
| `finalize_races_every_writer` | For every mutation class of §2.2, N pauses a transaction after its fence (the `AfterFence` fault seam). N+1's finalize waits, and the paused writer commits. A writer that begins after finalize fails `WriterFenced` with zero rows written. A pre-encoded commit that straddles finalize is encoded again under N+1's `F`. Runs on PostgreSQL and on SQLite (each database). |
| `negotiated_wire_both_directions` | Remote protocol: N+1 to N and N to N+1 select 1, including requests, replies, errors and streams. A synthetic `[2,2]` peer against `[1,1]` gets `Unsupported`, with zero effects. Restate: an N caller reaches N+1's handlers and gets version-1 replies. After a rollback an N+1 caller reaches N's handlers. A disjoint call changes nothing. |
| `object_sweep_crash_resume` | Objects and `LashTurn` outcomes written by N and by N+1 before finalize are all in N's format, and N reads them all. After finalize the synthetic sweep converts objects and survives a crash mid-sweep. Preflight lists the objects still at format 1. A kept N handler is refused typed by `_compat`. |
| `generation_handoff_rollback` | A foreign-`G` journal dispatches zero effects and parks. Signals that race a hand-off are delivered exactly once. A root admitted by a drive pinned to N runs on N+1, with no refusal and no `SubstrateLost`. A continuation N cannot decode keeps N+1's deployment and routes there. Rollback registers N at a fresh URI. Registering at a URI that serves another generation is refused. |
| `retention_delivery_rollback` | Checkpoints, attachments, referrer edges and fences, and obligations written by N+1 before finalize survive N's rollback, N's retention and GC, and a return to N+1: nothing is lost or delivered twice. A row with an unknown obligation kind stays outstanding and typed under N. |
| `history_after_finalize` | After finalize, N+1 still reads the history N wrote, through the permanent floor and not through `{F, newest}`. |
| `operator_json_contract` | Golden `--json` output and exit codes for every `lashctl` command, success and refusal. It is a single-binary test in `crates/lashctl/tests/`. |

`just e2e-rolling` runs the ADR 0106 §6 choreography over the same two
binaries: migrate, half roll, rollback, roll, drain, retire and finalize,
with zero refusals and zero duplicate effects. The judged runbook is
`runbooks/rolling-upgrade/`. CI requires the gate on every PR that touches a
registered surface.

### 7. What FIG-3846's cut checklist adds

ADR 0106 §8's five cut steps stand, with the corrected pointer for the bump
gate. The cut commit also:

1. **Resets every descriptor.** The PostgreSQL component goes to 1 with
   `min_reader` 1. The three SQLite components go to 1/1. `F` goes to 1 with
   writable `[1,1]`. `REMOTE_PROTOCOL_VERSION` goes from 100 to 1.
   `RESTATE_WIRE_VERSION`, every object family format, every `_compat`
   baseline, `LASH_TURN_OUTCOME_FORMAT_VERSION`,
   `MODULE_ARTIFACT_ENVELOPE_VERSION`, `SQLITE_BLOB_ENVELOPE_VERSION`,
   `ARTIFACT_REFERRER_KINDS_VERSION`, `OBLIGATION_LEDGER_VOCABULARY_VERSION`,
   `TRACE_SCHEMA_VERSION` and `LASHCTL_JSON_SCHEMA_VERSION` all go to 1.
   `PROCESS_CURSOR_VERSION` becomes `lashpc1`.
2. **Removes every pre-cut path.** It deletes the retired-version lists
   (`RETIRED_PROCESS_CURSOR_VERSIONS` and its kin), every `RECORD_UPCASTERS`
   and object `upcast_n1` row, every `WRITER_PINS` row, and every expand step
   in `EXPAND_MIGRATIONS`.
3. **Resets `G`'s inputs.** Every D row and `JOURNAL_LOGIC_EPOCH` go to 1.
   The commit recomputes `G`, and a check asserts that every `_g<G>` name
   the build binds derives from the new `G`.
4. **Registers every surface.** It adds §5's new rows, registers
   `PROCESS_CURSOR_VERSION` (now C), deletes the stale
   `QUEUED_WORK_CLAIM_LEASE_ENCODING_VERSION` exclusion, and makes
   `scripts/check_format_registry.py` strict.
5. **Restores the strict gates.** It brings back the version-bump gate and
   the upgrade-path declaration from `7233634ca8^`, with empty baselines,
   and makes `scripts/check-guarded-transactions.py` required.
6. **Captures after the reset.** `capture_release_fixtures.py --tag v1.0.0`
   also captures a PostgreSQL catalog, each SQLite database, the Restate
   object and outcome corpus, the remote-protocol message corpus, and the
   `lashctl --json` goldens.
7. **Proves the cut.** Phase A (§6) and `just e2e-rolling` pass on the cut
   commit itself, and FIG-3806's guide passes its command coverage check.
8. **Waits for the in-place changes.** The cut waits for ADR 0112's,
   ADR 0113's and ADR 0114's integrations and for every lane of §9. All of
   them change stored shapes in place under the freeze. FIG-3946 has landed
   (`671a616419`).

### 8. Before 1.0 and after 1.0

The study's split is confirmed, with three corrections: two additions and
one move.

**In the 1.0 binary, before the cut:**

- the descriptor, the stamps, admission, the tolerant shape check and the
  typed refusals (FIG-4043, extended to SQLite and to Restate objects);
- the writer fence (FIG-3800 part A);
- the Restate call envelope, `_compat`, the selected encoder and the
  versioned `RootOutcome` (FIG-4041's 1.0 half);
- **added:** removing the `drive_version` gate (§3.1) and the registration
  guard (§3.5);
- remote-protocol negotiation (FIG-3804);
- the per-surface obligations of §5 that change 1.0 code;
- `lashctl` with pinned JSON and exit codes (FIG-3847);
- Phase A (FIG-3805 A);
- the cut (FIG-3846).

**Alongside 1.0:** the operator guide (FIG-3806).

**After 1.0, each before its first use:**

- FIG-3800 B: `lashctl finalize`, the retired-deployment check and the hold
  flag;
- FIG-3801: SQLite migrate-on-open after a backup;
- FIG-3802: decoder coverage, the permanent history upcasters and the
  registry gate;
- FIG-3817: backfill and contract;
- FIG-4041's second half: the `upgrade` handlers, the sweep and the
  preflight;
- FIG-3805 B: the rolling E2E on the real `v1.0.0` image.

**Moved:** the study lists the object sweep proof under the object wire
contract. The sweep itself is N+1's code. Only its synthetic form, in
Phase A, is pre-cut.

Finalize commits the irreversible `F` boundary after the drain and the
retirement. Object sweeps and backfills then run to completion, and contract
waits for their ledger. Nothing rewrites an incompatible object while
rollback is still promised.

### 9. Lanes and file ownership

Eleven lanes. Each lands on `main` by itself when its done-when holds. There
is no integration branch, because every lane keeps the workspace compiling.
A lane edits only the files it owns and the regions it names in shared files.
A change another lane needs goes to that lane's owner. Each lane
regenerates the BUILD files of the crates whose files it adds.

**In-flight cutovers.** ADR 0112 (`fig-1628/cutover`, C0 at `a34c38117e`)
assigns whole crates by glob. ADR 0113 (`fig-4031/referrers`, R0 at
`b76409605c`) and ADR 0114 (`fig-433/stopped-partials`) carve files out of
those globs. This record does the same, with one difference. The PostgreSQL
and SQLite crates are being restructured by ADR 0112's store lanes right now,
so before those lanes land this arc claims only regions in them, never whole
files. Elsewhere, a file a lane below owns outright is carved out of ADR
0112's globs, and ADR 0112's lanes do not edit it. For a shared file, the
in-flight lane stays the owner, this arc's lane writes only its named region,
and whichever change lands later rebases and resolves.

**Contract first.** Lane **C0** lands first, and every other lane stacks on
it. It holds every shared type and constant:

- `VersionRange`;
- `ComponentId`, `CompatDescriptor`, `CompatStamp`, `admit`,
  `CompatRefusal` and `DESCRIPTORS`;
- the `StoreError::Incompatible` and `WriterFenced` variants (added beside
  the old variants, which the owning lanes delete);
- the `F`-epoch API in `fleet_format.rs`, and `ReadWindow`'s permanent
  history floor (§5);
- `Negotiation`, `answer` and `Negotiated`;
- `Call`, `Reply`, `RESTATE_WIRE_VERSION` and the `_compat` record type;
- their registry rows.

| Lane | Scope | Owns | Done when | Edges |
|---|---|---|---|---|
| **C0** contract | §1.1, §1.5, §2.1 and the history floor of §5, §3.1–3.2 types, §4 types | new `crates/lash-sansio/src/compat.rs`; new `crates/lash-core-store/src/compat.rs`; `crates/lash-core-store/src/store/fleet_format.rs`; new `crates/lash-remote-protocol/src/negotiation.rs`; new `crates/lash-restate/src/compat.rs`. Regions: the `mod` and re-export lines of `lash-sansio`, `lash-core-store`, `lash-remote-protocol` and `lash-restate`; `crates/lash-core-store/src/store/error.rs` (the two variants); `scripts/versioned-surfaces.toml` (its rows) | `kiln clippy` green for the whole workspace; unit tests `version_range_select_is_the_highest_common_version`, `version_range_refuses_empty_and_zero`, `admit_truth_table` (every §1.3 arm), `negotiation_bootstrap_json_is_frozen`, `restate_call_and_reply_json_is_frozen` pass | SOFT on ADR 0112 C0, ADR 0113 R0 and ADR 0114 C0 (`error.rs` and `lib.rs` regions; rebase) |
| **L1** store descriptor | §1.2–1.5 on both stores (FIG-4043) | New `crates/lash-sqlite-store/src/compat.rs` and `crates/lash-postgres-store/src/postgres/schema_compat_tests.rs`. Regions: `crates/lash-postgres-store/schema.sql` (the `lash_schema_versions` and `lash_fleet_format` blocks and the seed rows); `crates/lash-postgres-store/src/postgres/schema.rs` (the version gate in `ensure_schema`, `supported_version`, `version_mismatch_error`); `crates/lash-postgres-store/src/postgres/schema_shape.rs` (the classification of unexpected objects); `crates/lash-postgres-store/src/postgres/fleet_format.rs` (admission); `crates/lash-postgres-store/src/postgres/migrate.rs` (stamp writes carry `min_reader`; the expand-safety test); `crates/lash-postgres-store/src/lib.rs` (the component descriptor); `crates/lash-sqlite-store/src/schema.rs` (`apply_versioned_schema_tx`, `stamp_deployment_metadata`, the `lash_compat` DDL in each database, the `fleet_format` DDL deleted); `crates/lash-sqlite-store/src/fleet_format.rs` (deleted into `compat.rs`); `crates/lash-sqlite-store/src/lib.rs` (`mod` line); `crates/lash-core-store/src/store/error.rs` (delete `SchemaVersionOutOfRange` and `FleetFormatOutsideWritableRange`) | tests `postgres_opens_an_expanded_catalog_under_its_floor`, `postgres_refuses_a_raised_floor_typed`, `postgres_refuses_each_unsafe_addition`, `postgres_refuses_a_populated_catalog_without_a_stamp`, `sqlite_opens_each_expanded_database_under_its_floor`, `sqlite_refuses_a_raised_floor_typed`, `sqlite_refuses_a_partially_advanced_set`, `every_expand_step_passes_the_previous_tolerant_check` pass; `rg 'user_version' crates/lash-sqlite-store/src` finds no admission read; `kiln clippy` green | HARD on C0. SOFT on ADR 0112's PostgreSQL and SQLite lanes, ADR 0113 P and S, and ADR 0114 S (every file above is a region; the later landing rebases) |
| **L2** PostgreSQL writer fence | §2.2–2.4 on PostgreSQL (FIG-3800 A) | new `crates/lash-postgres-store/src/postgres/guarded_tx.rs`; the transaction-entry lines of every mutating site under `crates/lash-postgres-store/src/`; the `AfterFence` seam in `crates/lash-postgres-store/src/testing.rs`; new `scripts/check-guarded-transactions.py` and `scripts/guarded-transaction-readonly.txt`; the check's wiring in `scripts/ci_plan.py` | the lint passes with zero unguarded mutation sites; tests `pg_fence_orders_a_writer_before_finalize`, `pg_fence_refuses_a_writer_after_finalize_with_zero_writes`, `pg_fence_encodes_again_when_f_moves`, `pg_fence_retries_contended_with_a_fresh_read` pass; `scripts/perf_guard_budgets.json` budgets hold; `kiln clippy` green | HARD on L1. HARD on ADR 0112's, ADR 0113's and ADR 0114's integrations landing on `main`: they restructure the crate and add transaction sites the fence must cover |
| **L3** SQLite writer fence | §2.2–2.4 on SQLite (FIG-3800 A) | `crates/lash-sqlite-store/src/conn.rs`; the fence half of `crates/lash-sqlite-store/src/compat.rs`; the SQLite half of the lint | tests `sqlite_fence_refuses_a_writer_after_finalize_in_each_database`, `sqlite_fence_readmits_a_stamp_migrated_by_another_process`, `sqlite_migration_takes_every_database_exclusively_in_order` pass; the lint passes; `kiln clippy` green | HARD on L1 and on ADR 0112's integration (its SQLite lane rebuilds connection ownership). SOFT on ADR 0113 and ADR 0114 (their writes go through `write`) |
| **L4** Restate compatibility | §3 | `crates/lash-restate/src/{object_state.rs,effect_group.rs,durable_wait.rs,process_attach.rs,engine.rs,ingress.rs}`; `crates/lash-restate/src/effect_group/{protocol.rs,payload.rs}`; `crates/lash-restate/src/process/{workflow.rs,mod.rs}`; new `crates/lash-restate/src/tests/compat_on_the_double.rs`. Regions: `crates/lash-restate/src/session_driver.rs` (request and reply types, the two `drive_version` gates, the `outcome` state); `crates/lash-restate/src/effect_group/dispatch.rs` (shared-handler request types); `scripts/versioned-surfaces.toml` (its rows) | a test enumerates every handler the binder binds and asserts it takes `Call` and answers `Reply`; per family, a test shows `_compat` refusal with zero state change; `rg 'formats\.current' crates/lash-restate/src` finds no write; `pinned_older_drive_root_runs_on_the_newer_build` and `registration_refuses_an_endpoint_serving_another_generation` pass on the multi-deployment double; `kiln clippy` green | HARD on C0. SOFT on ADR 0112's runtime lane (`session_driver.rs` and `dispatch.rs` regions), on ADR 0113 X and ADR 0114 R (no shared file), and on L2 and L3 (the fresh `F` view) |
| **L5** remote negotiation | §4 (FIG-3804) | `crates/lash-remote-protocol/src/negotiation.rs` (logic); new `crates/lash-remote-protocol/src/negotiation_tests.rs`. Regions: `crates/lash-remote-protocol/src/lib.rs` (`Envelope`, decode, the constants); `crates/lash-remote-protocol/src/registry_errors.rs` (the refusal); every `Envelope::new` caller (`crates/lash-remote-protocol/src/{turn_input.rs,core_conversions/observations.rs}`, `examples/agent-workbench/src/main_sections/routes/host_streams.rs`) | tests `hello_answer_selects_the_highest_common_version`, `disjoint_ranges_answer_unsupported_before_any_decode`, `each_request_is_validated_against_the_local_range`, `replies_errors_and_streams_use_the_request_version` pass; `rg 'Envelope::new\b' crates examples` is empty; `kiln clippy` green, with `//crates/lash:ui_fixtures` if facade exports change | HARD on C0. SOFT on ADR 0112's runtime lane (glob owner) and ADR 0114 H (`turn_result.rs` is not touched) |
| **L6** `lashctl` | the operator CLI and §5's JSON contract (FIG-3847) | new `crates/lashctl/**`; `crates/lash-postgres-store/src/bin/lash_migrate.rs` (deleted). Regions: `crates/lash-postgres-store/BUILD.bazel` and `Cargo.toml` (the binary target deleted); `crates/lash-core-store/src/store/error.rs` (remedy texts); every `lash-migrate` spelling in `justfile`, `scripts/` and `runbooks/` | `lashctl` serves `migrate`, `drain`, `drain-status`, `end-drain`, `preflight` and `version`; `operator_json_contract` passes; `bazel query 'attr(name, "lash_migrate", //...)'` and `rg -w 'lash-migrate' crates scripts runbooks justfile` are empty; `kiln clippy` green | HARD on C0 (`version` prints the descriptors). SOFT on L1 (descriptor values), ADR 0112 (it re-homes the store ports the drain verbs call) and ADR 0113 (it deletes the session-delete drain count, which re-blesses the goldens) |
| **L7a** surfaces now | §5 rows that no in-flight cutover owns: the SQLite blob envelope, attachment owners, trace readers, process cursors | Regions: `crates/lash-sqlite-store/src/{codec.rs,lib.rs}` (`StoredBlobEnvelope`); `crates/lash-core-store/src/store/attachment_manifest.rs` (`decode_attachment_owner`); the GC call sites in `crates/lash-{sqlite,postgres}-store/src/**/attachments.rs`; `crates/lash-trace/src/**`; `crates/lash-sansio/src/process_cursor.rs`; `scripts/versioned-surfaces.toml` (its rows) | tests `blob_envelope_refuses_an_unknown_version_and_keeps_the_bytes`, `gc_keeps_an_attachment_whose_owner_does_not_decode`, `trace_reader_skips_unknown_kinds_and_counts_them`, `cursor_is_minted_at_the_fleet_version` pass; `kiln clippy` green | HARD on C0. SOFT on ADR 0112's SQLite, PostgreSQL and runtime lanes (regions) |
| **L7b** surfaces after ADR 0113 | §5 rows in files ADR 0113 owns: the module artifact envelope, the obligation vocabulary, referrer GC | `crates/lashlang/src/artifact.rs` (the envelope region); `crates/lash-core-store/src/store/obligation.rs` (`from_label` and kind decode); `crates/lash-core-execution/src/runtime/drive/relay.rs` (stall, never settle, an unknown kind); `crates/lash-core/src/runtime/artifact_cleanup.rs` (stop at an undecodable edge) | tests `artifact_verifies_under_its_stored_family`, `artifact_refuses_an_unknown_family_typed`, `relay_stalls_an_unknown_obligation_kind_without_settling_it`, `cleanup_never_counts_an_undecodable_edge_as_absent` pass; `kiln clippy` green | HARD on C0 and on ADR 0113's integration landing on `main` (lane R owns all four files until then) |
| **L8** Phase A | §6 (FIG-3805 A) | new `crates/lash-upgrade-harness/**`; `runbooks/rolling-upgrade/**`; the `e2e-rolling` recipe in `justfile`; its wiring in `scripts/ci_plan.py`. Regions: the `synthetic-next` blocks beside the moved constants in `crates/lash-postgres-store/src/{lib.rs,postgres/migrate.rs}`, `crates/lash-sqlite-store/src/schema.rs`, `crates/lash-core-store/src/store/fleet_format.rs`, `crates/lash-core-store/src/session_graph.rs`, `crates/lash-remote-protocol/src/negotiation.rs`, `crates/lash-restate/src/{compat.rs,effect_group/protocol.rs,process/admission.rs}` and `crates/lashlang/src/runtime/vm/continuation.rs`, and the feature lines of those crates' `Cargo.toml` and BUILD files | the eight `phase_a` tests and `just e2e-rolling` pass; CI requires them on registered-surface PRs | SOFT to start (harness, node binary, bring-up). Each leg is HARD on its lane: `expanded_store_rollback` and `skipped_compatibility_release_refused` on L1; `finalize_races_every_writer` on L2 and L3; `negotiated_wire_both_directions` on L4 and L5; `object_sweep_crash_resume` and `generation_handoff_rollback` on L4; `retention_delivery_rollback` on L7a and L7b; `history_after_finalize` on L1 |
| **L9** operator guide | FIG-3806 | new `docs/operations/deploying-and-upgrading.md`; a coverage check that every command the guide names runs in `runbooks/rolling-upgrade/` | the guide exists and the check passes | HARD on L6 (command names). SOFT on L8 |

**The cut** (FIG-3846, §7) is not a lane. It is the orchestrator's
release-gate commit, and it is HARD on every lane above and on the three
cutover integrations.

**Which lanes wait.**

- **Now,** from `main`: C0, then L1, L4, L5, L6 and L7a in parallel, and
  L8's harness skeleton.
- **After ADR 0112's integration:** L3.
- **After ADR 0112's, ADR 0113's and ADR 0114's integrations:** L2.
- **After ADR 0113's integration:** L7b.
- **After L6:** L9.

## Where the study is refined

- **Schema floors are checked at open, and on SQLite at every write.** The
  study checks the stamp at open and at transaction admission. On PostgreSQL
  the per-transaction fence reads only `F`. A floor rises only in a contract
  step, which runs only after the release below the floor has been
  finalized. That finalize already moved `F` and fenced every writer the
  floor would exclude. So the extra check would buy nothing and cost a
  second row read. SQLite checks both, because it is the same local row.
- **`F` moves at every compatibility release.** The study keeps `F` as the
  release compatibility epoch. This record adds that it moves even when no
  format changed. Otherwise a release with only schema and wire changes
  would have no fence for the old release's writers.
- **`_compat` drops `epoch` and `writer_format`.** An object versions only
  its own family. `F` lives in SQL, and the build maps `F` to the family's
  format through `WRITER_PINS`, so an epoch stored in the object would be a
  second source of truth. `format` is kept as the floor of the formats
  present, which is what sweep progress and the preflight measure.
- **One Restate wire version.** The study speaks of wire ranges on "shared
  object requests". One invocation calls several services: a drive calls
  `LashTurn`, `EffectGroupIndex` and `LashDurableWaitIndex`. So one
  negotiated number per call is what a caller can state.
  `EFFECT_GROUP_WIRE_VERSION` is subsumed. The envelope also covers the
  workflows' shared handlers and the session handlers, which the study's
  table reaches only through `RootOutcome`.
- **No unknown-field preservation in 1.0.** The study allows either
  preserving unfamiliar optional data on rewrite or forbidding its emission
  under the old format. This record picks forbidding, by the law. 1.0 then
  carries no round-trip machinery, and after finalize no 1.0 code runs.
- **SQLite's `user_version` stops being the stamp.** The study keeps it
  beside a new record. A second stamp would be a second source of truth, so
  `lash_compat` replaces it.
- **Two findings are added:** the cross-build `drive_version` gate (§3.1)
  and the forced registration (§3.5). Both would break a roll before any
  format changed.
- **Two retry classes are distinguished.** The study retries contention with
  a fresh check. This record also separates a moved-but-writable `F` (encode
  again, retry once) from a fenced one (terminal).
- **The history floor lands before the cut.** The study leaves permanent
  history reads to FIG-3802 after 1.0, and the 1.0 binary itself reads only
  version 1. But Phase A's `history_after_finalize` runs a synthetic N+1
  built from the 1.0 tree, so the read window's history floor must exist in
  that tree. It is a small change to C0's file. The upcasters stay post-1.0.

## Consequences

- Every mutating transaction pays one fence. On PostgreSQL that is one
  round trip and one shared row lock; on SQLite it is one local read. Until
  1.1's finalize, the fence never refuses and never re-encodes.
- A 1.0 pod survives a 1.1 expand, a 1.1 roll and a rollback to itself. It
  stops writing the moment 1.1 finalizes, with `WriterFenced`, and it never
  writes a row after that.
- Every Restate handler reads one more key and pays one more envelope. The
  envelope's outer shape and `_compat`'s shape are frozen forever.
- Operators have one binary, `lashctl`, whose `--json` output and exit codes
  are a public contract from 1.0.
- `REMOTE_PROTOCOL_VERSION` restarts at 1. Hosts perform a `Hello` on every
  connection.
- A host must serve each build at its own Restate endpoint URI. Rolling pods
  behind one URI is refused at registration.
- The cut gains a strict gate set: the bump gate, the upgrade-path
  declaration, the registry and the guarded-transaction lint. It also gains
  a two-binary release gate that runs on every registered-surface PR.
- Stored shapes change in place under the freeze: `lash_schema_versions`
  gains `min_reader`, SQLite gains `lash_compat` and loses `fleet_format`,
  Restate objects gain `_compat`, `LashTurn`'s outcome is stamped, and
  SQLite blobs and module artifacts gain envelopes. Old stores are refused
  and recreated. A journal in flight at the deploy drains on the build that
  wrote it (ADR 0106).
