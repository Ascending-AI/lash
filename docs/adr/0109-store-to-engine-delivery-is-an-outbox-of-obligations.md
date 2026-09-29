# 0109: Store→engine delivery is an outbox of obligations

## Status

Accepted 2026-09-27 (FIG-3600 S8). It records Sam's S8 rulings of that date.
Amended 2026-09-29 (FIG-4125, item 14): the foundation and the named lanes below
are implemented. The per-ledger slices under *Slice plan* retain their own
status. Landed: S8-D, the two-phase
session delete (§4); S8-S, scope close on the root row (§3, §6); S8-C,
control intents; S8-P, parent-end plans (§3); S8-I, ingress (§3, §7);
S8 process start (FIG-3918); and `ArtifactCleanup` delivery. Generation
drain reads obligations of one build generation; it is not deployment drain
([ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md)).

**Process start is implemented.** Registration arms `ProcessStart` on each
Lash-executed process in the same SQL transaction. The relay submits its
current segment to the engine and settles the obligation after ingress accepts
it. It is also the only delivery path: the registering transaction's own
attempt is `deliver_now` against the armed row, and a journaled engine that
must carry its own submission context (Restate's process-start workflow send,
with its execution context and causal invocation) claims the row, sends, and
settles through the same ledger — no path submits a start outside a claim.
Restate coalesces repeated `run` submissions by workflow key; its workflow
`run` handler rejects an idempotency-key header, so the relay's attempt key
remains local to Lash's claim. The obligation is delivered once and nothing
re-arms it. Lost or paused runs remain the engine recovery pass's
responsibility: its lost-run scan resubmits the current segment of a live
process whose run Restate no longer holds, and that segment's admission ends a
started process `SubstrateLost` (ADR 0110 §2).

**S8-T (process terminal publication) is implemented.** Every transaction
that makes a process terminal arms the row's `ProcessTerminal` obligation;
the Restate segment that stored the terminal publishes it in its own journal
and settles the row once the publication is durable, and the relay publishes
through the root workflow's `complete_terminal` what no segment did. The
process park pass kills a paused segment of a terminal process once its
publication is delivered.

Amends [ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
O2 and O3 (the mechanism behind "reconcile every unacknowledged intent" and
"retry needs an explicit attempt policy") and
[ADR 0108](0108-a-process-lives-until-a-scope-its-start-could-reach.md) §5
(amendment text in §6 below). [ADR 0080](0080-substrate-attestation-is-not-a-lease-short-circuit.md)
stands: the leader lease is load control, never a fence.

Amended 2026-09-28 (FIG-3927), implemented: [ADR
0101](0101-one-session-ingress-carries-every-admitted-item.md)'s claim-free
amendment replaces the drive's claim of an ingress row with the root's
admission write (§3). The obligation/relay claim machinery this ADR owns is
unchanged: `obligation_claim_token` and the `claimed` state are relay claims,
not turn-selection claims.

## 1. Interface (frozen; slices build against this)

### 1.1 Obligation columns

A ledger row that owes the engine an effect carries these columns. The row
is the obligation: no side table. The same names are used on every ledger.
A ledger that owes two kinds carries a second, prefixed family of the same
columns: `processes` owes `ProcessStart` on `start_obligation_*` beside
`ProcessTerminal` on `obligation_*`, and the two families are independent —
a row's start settles while its terminal obligation is still unarmed, and
arming or settling either never touches the other.

| Column | SQLite | PostgreSQL | Meaning |
|---|---|---|---|
| `obligation_id` | `TEXT` | `TEXT` | Stable id, minted when the row is armed. Unique. `NULL` iff the row owes nothing. |
| `obligation_state` | `TEXT` | `TEXT` | `due`, `claimed`, `delivered`, `stalled`; `NULL` iff the row owes nothing. |
| `obligation_attempts` | `INTEGER NOT NULL DEFAULT 0` | `INTEGER NOT NULL DEFAULT 0` | Claims taken since the row was armed or re-armed. |
| `obligation_due_at_ms` | `INTEGER` | `BIGINT` | When a relay may next take the row: the backoff's next attempt while `due`, the claim's expiry (claimed-until) while `claimed`; `NULL` otherwise. |
| `obligation_claim_token` | `TEXT` | `TEXT` | Set iff `claimed`. Every settling write compares it. |
| `obligation_stall_reason` | `TEXT` | `TEXT` | `attempts_exhausted`, `refused`, `undecodable`; set iff `stalled`. |
| `obligation_last_error` | `TEXT` | `TEXT` | The last failed attempt's detail; cleared on `delivered`. |
| `obligation_settled_at_ms` | `INTEGER` | `BIGINT` | Store-clock time of `delivered` or `stalled`; `NULL` otherwise. |

The decisions' `next_attempt_at_ms` and `claimed_until_ms` are one column,
`obligation_due_at_ms`: a row is never both waiting and claimed, so two
columns would be two sources of truth for "when may a relay take this row".

One CHECK per family, `ck_<table>_obligation` (`ck_processes_start_obligation`
for the `processes` start family), pins the combinations:

```sql
(obligation_state IS NULL AND obligation_id IS NULL AND obligation_due_at_ms IS NULL
   AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL)
OR (obligation_state = 'due' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL
   AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL)
OR (obligation_state = 'claimed' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NOT NULL
   AND obligation_claim_token IS NOT NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NULL)
OR (obligation_state = 'delivered' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL
   AND obligation_claim_token IS NULL AND obligation_stall_reason IS NULL AND obligation_settled_at_ms IS NOT NULL)
OR (obligation_state = 'stalled' AND obligation_id IS NOT NULL AND obligation_due_at_ms IS NULL
   AND obligation_claim_token IS NULL AND obligation_settled_at_ms IS NOT NULL
   AND obligation_stall_reason IN ('attempts_exhausted', 'refused', 'undecodable'))
```

Three indexes per family (`lash_` prefix on PostgreSQL;
`idx_processes_start_obligation_*` for the `processes` start family):

```sql
CREATE UNIQUE INDEX idx_<table>_obligation_id ON <table>(obligation_id);
CREATE INDEX idx_<table>_obligation_due ON <table>(obligation_due_at_ms, obligation_id)
    WHERE obligation_state IN ('due', 'claimed');
CREATE INDEX idx_<table>_obligation_stalled ON <table>(obligation_id)
    WHERE obligation_state = 'stalled';
```

The due read is `WHERE obligation_state IN ('due','claimed') AND
obligation_due_at_ms <= :now ORDER BY obligation_due_at_ms, obligation_id
LIMIT :n`, which also retakes a lapsed claim. PostgreSQL adds `FOR UPDATE SKIP
LOCKED`. Claiming sets `claimed`, a fresh token, `attempts + 1` and
`due_at = now + claim_ttl`. Rows are never read by scanning the table.

### 1.2 Kinds

`ObligationKind` (lash-core-store): `Ingress`, `ControlIntent`, `ScopeClose`,
`ParentEnd`, `SessionDelete`, `ProcessStart`, `ProcessTerminal`. Its label
(`ingress`, `control_intent`, `scope_close`, `parent_end`, `session_delete`,
`process_start`, `process_terminal`) is the metric label and the
drain-status key.

`ObligationKey` names the row an obligation lives on, one variant per kind:
`Ingress { session_id, item_id }`, `ControlIntent { intent_id }`,
`ScopeClose { session_id, root }`, `ParentEnd { parent_kind, parent_id }`,
`SessionDelete { session_id }`, `ProcessStart { process_id }`,
`ProcessTerminal { process_id }`.

`process_wake_deliveries` is the template and already has this shape under
its own names (`pending`/`enqueuing`/`enqueued`/`discarded`, `attempts`,
`next_attempt_at_ms`, `claim_token`, `discard_reason`); S8 does not rename it.

### 1.3 Store half: the ledger

In `lash_core_store::store::obligation`:

```rust
pub struct ObligationId(String);          // opaque, stable
pub struct ClaimToken(String);
pub enum ObligationState { Due, Claimed, Delivered, Stalled }
pub enum StallReason { AttemptsExhausted, Refused, Undecodable }

pub struct ClaimedObligation {
    pub id: ObligationId,
    pub token: ClaimToken,
    pub attempts: u32,                     // after this claim
    pub key: Result<ObligationKey, UndecodableObligation>,
}
pub struct UndecodableObligation { pub detail: String }

pub enum ObligationSettlement {
    Delivered,
    Retry { due_at_ms: u64, error: String },
    Stall { reason: StallReason, error: String },
}
pub enum SettleOutcome { Applied, ClaimLost }

pub struct StalledObligation {
    pub kind: ObligationKind, pub id: ObligationId, pub reason: StallReason,
    pub attempts: u32, pub last_error: Option<String>, pub stalled_at_ms: u64,
}

#[async_trait]
pub trait ObligationLedger: Send + Sync {
    fn kind(&self) -> ObligationKind;
    /// Arm `key`'s row outside a producer transaction (the leader's repair
    /// pass): only a row that owes nothing is armed. `None` if the row is
    /// missing or already carries an obligation.
    async fn arm(&self, key: &ObligationKey, now_ms: u64)
        -> Result<Option<ObligationId>, StoreError>;
    /// Due rows, oldest due first; a row whose key fails to decode is
    /// returned claimed with `key: Err`, never failing the page.
    async fn claim_due(&self, now_ms: u64, claim_ttl_ms: u64, limit: NonZeroUsize)
        -> Result<Vec<ClaimedObligation>, StoreError>;
    /// Claim one `due` row by id (immediate delivery). `None` if it is not due.
    async fn claim(&self, id: &ObligationId, now_ms: u64, claim_ttl_ms: u64)
        -> Result<Option<ClaimedObligation>, StoreError>;
    /// Settle a claim; `ClaimLost` when the token no longer matches.
    async fn settle(&self, id: &ObligationId, token: &ClaimToken,
        settlement: ObligationSettlement, now_ms: u64) -> Result<SettleOutcome, StoreError>;
    /// `stalled` → `due` at `now_ms`, attempts reset. `false` if not stalled.
    async fn rearm(&self, id: &ObligationId, now_ms: u64) -> Result<bool, StoreError>;
    async fn list_stalled(&self, after: Option<&ObligationId>, limit: NonZeroUsize)
        -> Result<Vec<StalledObligation>, StoreError>;
    async fn count_stalled(&self) -> Result<u64, StoreError>;
}
```

Each store answers `StoreSet::obligation_ledger(kind) -> Arc<dyn
ObligationLedger>` over per-table statements in
`lash_store_sql::obligation`. A slice decodes what the key names inside its
`deliver`. A producer arms inside its own transaction with the same `arm`
statement (`obligation_state = 'due'`, `obligation_due_at_ms = now`, a minted
id), through the store's `arm_obligation` helper on that transaction. A
producer whose transaction stamps its rows with the database clock (the
PostgreSQL process registry) arms its row due at once (instant 0) rather than
at that instant: the relays claim on their host clock, and a host clock behind
the database must not defer the row's first attempt.

### 1.4 Engine half: the relay

In `lash_core::runtime::drive::relay`:

```rust
pub enum DeliveryFailure { Retryable(String), Refused(String), Undecodable(String) }

pub struct RelayPolicy {
    pub base_backoff_ms: u64,              // 1_000
    pub max_backoff_ms: u64,               // 900_000 (15 min)
    pub attempt_ceiling: NonZeroU32,       // 16, per kind, host-configurable
    pub claim_ttl_ms: u64,                 // 60_000
}
// next due = now + min(base << (attempts - 1), max)

#[async_trait]
pub trait ObligationRelay: Send + Sync {
    fn ledger(&self) -> &dyn ObligationLedger;   // its kind is the relay's kind
    fn policy(&self) -> RelayPolicy;
    /// Idempotent under a repeated id: the engine dedupes on a key derived
    /// from `id`.
    async fn deliver(&self, id: &ObligationId, key: &ObligationKey)
        -> Result<(), DeliveryFailure>;
}

pub enum RelayVerdict { Delivered, Retried { due_at_ms: u64 }, Stalled(StallReason),
    ClaimLost, NotDue }
pub struct RelayPass { pub claimed: usize, pub delivered: usize, pub retried: usize,
    pub stalled: usize, pub claim_lost: usize }

/// Immediate delivery of a producer's own commit: claim by id, deliver, settle.
pub async fn deliver_now(relay: &dyn ObligationRelay, id: &ObligationId,
    clock: &dyn Clock) -> Result<RelayVerdict, StoreError>;
/// One bounded due-claim pass.
pub async fn relay_due(relay: &dyn ObligationRelay, clock: &dyn Clock,
    limit: NonZeroUsize) -> Result<RelayPass, StoreError>;
```

The reconcile tick holds `&[Arc<dyn ObligationRelay>]`: one relay per
`ObligationKind`, in `ObligationKind::ALL` order, assembled by
`lash_core::runtime::drive::obligation_relays` from the parts a core
resolves (the backend, the catalog, the session engine, the scope owner, the
process registry and port, the session administration). A store set arms
every kind, so no kind's relay is left to a host to wire: `LashCore` runs all
of them on every builder path, the drive's close step and every close verb
deliver a root's scope close through the backend's `ScopeClose` ledger, and a
core that cannot supply some kind's delivery refuses to build with
`EmbedError::ObligationRelayUnavailable { kind, need }` (FIG-3888).

Settlement rule, applied by both entry points: `Ok` → `Delivered`;
`Refused` → `Stall(refused)`; `Retryable` → `Stall(attempts_exhausted)` when
`attempts >= ceiling`, else `Retry` at the backoff; `Undecodable`, or a key
that does not decode, → `Stall(undecodable)`. `ClaimLost` is counted, not an
error: another relay or the deliver's own transaction settled the row.
A deliver may settle its row itself inside the transaction that performs the
effect (scope close does); the relay's settle then answers `ClaimLost`.

### 1.5 Stalled surfacing

- **Ingress.** `lash::TurnStatus::Stalled(StalledDelivery)` on the send's
  outcome, carrying the `StalledObligation` fields. The ingress slice adds it.
- **Drain status.** `DeploymentDrainStatus::stalled_obligations:
  BTreeMap<ObligationKind, u64>`. `drained()` is false while any is non-zero.
  `GenerationDrainStatus` (FIG-3799), the per-build-generation read an
  operator polls after `LashCore::drain_generation`, carries the same map
  and the same rule beside the generation's live processes, parked
  processes and parked turns. It also counts the closing sessions (§4),
  which no drain outlives either: until a session's physical delete revokes
  them, the turn-control waits of the roots its close ended stay registered
  with the engine on whichever build ran them.
- **Metrics.** Counter `lash.obligation.attempts{kind, outcome}` with
  outcome `delivered | retried | stalled | claim_lost`; gauge
  `lash.obligations.stalled{kind}` (written by `drain_status`); gauges
  `lash.recovery_leader{name}` (1 while this process leads) and
  `lash.recovery_leader.term{name}`. A Prometheus exporter renders them
  `lash_obligation_attempts_total` and so on.
- **Verbs.** `LashCore::stalled_obligations(kind, after, limit)` and
  `LashCore::rearm_obligation(kind, &ObligationId)`. Re-arm is explicit;
  nothing re-arms automatically.

### 1.6 Leader lease

One row per (storage, engine authority) in `lash_recovery_leader` (SQLite:
`recovery_leader` in the core catalog):

```sql
CREATE TABLE lash_recovery_leader (
  name TEXT PRIMARY KEY,            -- the engine authority's lease name
  holder_id TEXT NOT NULL,          -- host:uuid, fresh per process
  generation_rank BIGINT NOT NULL,  -- host-supplied, higher = newer build
  term BIGINT NOT NULL,             -- +1 per holder change
  elected_at_ms BIGINT NOT NULL,
  expires_at_ms BIGINT NOT NULL);
```

`:now` is the database clock, read in the same transaction: PostgreSQL
`(extract(epoch from clock_timestamp())*1000)::bigint`, SQLite
`CAST(unixepoch('subsec')*1000 AS INTEGER)` inside `BEGIN IMMEDIATE`. The
statements below are identical on both backends once rendered.

```sql
-- acquire
INSERT INTO recovery_leader AS l (name, holder_id, generation_rank, term, elected_at_ms, expires_at_ms)
VALUES (:name, :me, :rank, 1, :now, :now + :ttl)
ON CONFLICT (name) DO UPDATE SET holder_id = excluded.holder_id,
  generation_rank = excluded.generation_rank, term = l.term + 1,
  elected_at_ms = excluded.elected_at_ms, expires_at_ms = excluded.expires_at_ms
WHERE l.expires_at_ms < :now
   OR (l.generation_rank < excluded.generation_rank AND l.elected_at_ms + :min_tenure < :now)
RETURNING term, holder_id;          -- no row: read the current holder
-- renew
UPDATE recovery_leader SET expires_at_ms = :now + :ttl
WHERE name = :name AND holder_id = :me AND term = :term AND expires_at_ms >= :now RETURNING term;
-- resign: expire the row, never delete it, so the term stays monotone
UPDATE recovery_leader SET expires_at_ms = :now - 1
WHERE name = :name AND holder_id = :me AND term = :term AND expires_at_ms >= :now;
```

In `lash_core_store::store::recovery_leader`:

```rust
pub struct LeaseName(String);
pub struct HolderId(String);
pub struct LeaseClaim { pub name: LeaseName, pub holder: HolderId,
    pub generation_rank: i64, pub ttl_ms: u64, pub min_tenure_ms: u64 }
pub struct LeaseRow { pub holder: HolderId, pub generation_rank: i64, pub term: i64,
    pub elected_at_ms: i64, pub expires_at_ms: i64 }
pub struct LeaseAnswer { pub leader: bool, pub row: Option<LeaseRow>, pub db_now_ms: i64 }

#[async_trait]
pub trait RecoveryLeaderStore: Send + Sync {
    async fn acquire(&self, claim: &LeaseClaim) -> Result<LeaseAnswer, StoreError>;
    async fn renew(&self, claim: &LeaseClaim, term: i64) -> Result<LeaseAnswer, StoreError>;
    async fn resign(&self, name: &LeaseName, holder: &HolderId, term: i64) -> Result<bool, StoreError>;
    /// True where due claims must be leader-only (SQLite).
    fn due_claims_need_leader(&self) -> bool;
}
```

`StoreSet::recovery_leader() -> Arc<dyn RecoveryLeaderStore>` is required.

In `lash_core::runtime::recovery_lease`, `RecoveryLease` runs acquire or
renew on its own cadence and publishes a `Standing`. The host sets
`LashCoreBuilder::recovery_lease(RecoveryLeaseConfig { generation_rank,
timings })`:

```rust
pub struct RecoveryLeaseTimings { ttl: 15 s, renew_every: 5 s, renew_timeout: 2.5 s,
    trust_margin: 2 s, follower_retry: 5 s, follower_jitter: 0..500 ms, min_tenure: 30 s }
pub enum Standing { Leader { term: i64, trusted_until_ms: u64 }, Follower }
pub struct RecoveryDuties { pub leader: bool, pub due_claims: bool }
impl RecoveryLease {
    pub fn duties(&self, now_ms: u64) -> RecoveryDuties;
    pub async fn step(&self) -> Standing;     // one acquire-or-renew
    pub async fn resign(&self);
}
```

The lease is named `recovery:{EffectHost::turn_control_binding_id()}`: the
engine authority that owns the effect state, in the storage that holds the
row. `ReconcileParts` carries `duties: RecoveryDuties` and `relays:
&[Arc<dyn ObligationRelay>]`; the facade's driver fills both.

`trusted_until = renew start + ttl − trust_margin` on the host clock; a leader
whose trust lapsed is a follower until its next successful renew. Losing the
lease stops leader duties, never the host. The host sets `generation_rank`
(ADR 0014 lever; default 0) and resigns at shutdown and on drain.

A deployment has one holder for its whole life, and its election attempts
run on a task of their own, never in the tick that first asks for its
duties. A tick cancelled after the store granted the lease (a shutdown
aborting a recovery pass) would otherwise drop the only holder that could
renew or resign the row, which would then name a holder nobody runs until
its TTL lapses. The task resigns when the deployment goes away, whatever its
attempt answered.

### 1.7 Duties

| Duty | Who |
|---|---|
| Restate handlers; a deployment's own process executions | every deployment |
| Immediate delivery of the deployment's own commits (`deliver_now`) | every deployment |
| Due-obligation claims (`relay_due`) | every deployment on PostgreSQL (`SKIP LOCKED`); the leader on SQLite |
| Parks arm; rate-bounded repair scans; drain hand-over (FIG-3799); park-feed compaction; opt-in evidence retention | leader only |

Every duty stays idempotent under two overlapping leaders.

### 1.8 Detection bounds the sim asserts

With tick `T` = 10 s ±10% (so consecutive passes are at most 11 s apart),
the leader lease's TTL 15 s and follower retry 5.5 s, and the relay's
`claim_ttl` 60 s:

- **Immediate.** A producer's obligation is attempted before its call returns.
- **Lost immediate attempt.** Claimed by `due_at + T` on PostgreSQL, by
  `due_at + T + 20.5 s` on SQLite across a leader failover.
- **Lapsed claim.** Retaken by `claimed_at + claim_ttl + T` (71 s): the
  claim lapses at `claimed_at + claim_ttl`, and the first due pass at or
  after the lapse retakes it. The bound is exact, not padded: a pass that
  lands just before the lapse, followed by the longest interval, retakes the
  row a hair under it. It holds only while the interval keeps its cadence —
  a pass fires every `T` from the last, whatever the pass itself or a
  harness waiting on the engine spent — which is how the crash matrix
  measures it on both engines (FIG-3899).
- **Retryable failure.** Attempt `n + 1` at `min(2^(n−1) s, 15 min)` after
  attempt `n`, plus at most `T`; `stalled` after `attempt_ceiling` attempts
  (≈ 1 h 47 min at the defaults), never later.
- **Undecodable or refused.** `stalled` in the pass that claims it; later
  rows of the same page are still delivered.
- **Leader loss.** Leader duties resume within 20.5 s of the leader's death;
  a newer `generation_rank` takes over within `min_tenure + renew_every`
  (35 s), and the old leader stops within one renew.

## 2. Rationale

The prospect (`tasks/prospect-seam/`, verified in `verify/correctness-out.md`)
found every store→engine handoff written as "commit, then send
fire-and-forget", healed by per-deployment scans with no due time, no attempt
count and no ceiling: the terminal-root rescan revisits every root ever
recorded (≈ 43 h per cycle at 1M roots), 64 failing parent-end plans block
every later scope, a single undecodable plan fails the whole arm every tick,
open intents and paused drives retry forever, and each pod runs every arm.
Temporal commits the task in the state's transaction and relays it with
backoff (`temporal-lens.md` T1–T6); River and controller-runtime give
maintenance to one leader and keep resync as insurance (`river-k8s-lens.md`
F1–F8). `process_wake_deliveries` already has the right shape. SQL stays the
acceptance authority (`astra-restate-first/answer.md`): Restate executes work
and is not a second ingress.

## 3. Per-ledger mapping

| Kind | Table (both stores) | Armed by | Delivered when | Replaces |
|---|---|---|---|---|
| `Ingress` | `pending_turn_inputs`, `queued_work_batches` | the admission transaction | the root's admission write admitted the row, in the admission's own transaction; the engine accepting drive request `{obligation_id}:{attempt}` only holds the relay's claim | the drives arm |
| `ControlIntent` | `control_intents` | the verb's transaction | the engine half is applied, the intent settled, and its follow-on drive `intent:{id}` accepted | the intents arm; the `attempts` column (use `obligation_attempts`) |
| `ScopeClose` | `session_roots` | the root's terminal transaction | the close's own transaction | the scopes arm |
| `ParentEnd` | `parent_end_plans` | the plan's record | every child's cancel delivered or refused | the parent-end slot and the native worker sweep |
| `SessionDelete` | `session_meta` | the close intent's delivered settle | physical delete ran | caller-retried deletion |
| `ProcessStart` | `processes` (`start_obligation_*`) | the registration transaction | the engine accepted the process's current-segment `run` submission | the hosts' pending-process admission pass (`admit_pending_processes`) |
| `ProcessTerminal` | `processes` | the terminal transaction | the engine's terminal promise resolved | nothing (S-14 had no owner) |

At the ceiling an intent stalls and is written `Failed{retryable: false}`,
which unwedges admission; re-arm reopens it. The intent's acknowledgement
and its failure compare the obligation's claim token, so a delivery whose
claim lapsed and was retaken never settles the intent. A cancel's or fork's
delivery does not wait on its root's scope close: a child whose cancel keeps
failing is the scope close's to retry, never a reason to hold the session's
verb open. A parent-end plan records each
child that refused, so one unreachable child stalls its plan, never the rows
behind it. On SQLite `parent_end_plans` and `processes` live in the registry
file: they are armed in that file's transaction, not the catalog's.

**Ingress (S8-I).** The ingress rows live in `pending_turn_inputs` and
`queued_work_batches`, which together are the session's one logical ingress
(ADR 0101's FIG-3540 close-out); the ingress ledger is the two tables' ledgers composed, a
due page being the oldest due rows of both. A row's obligation id is
`ingress:{item_id}` (its `ti:` input id or `qwb:` batch id), derived rather
than read back. The engine accepting the relay's ask does not deliver it:
the root's admission write — the row's selection — delivers it, in the
admission's own transaction, whatever state the obligation was in
(FIG-3927). The relay's claim so covers
only the ask and the admission after it. Its ask is the drive request
`{obligation_id}:{attempt}` (`ingress:{item_id}:{attempt}`), the attempt
being the claim's: the engine dedupes a request id against an invocation it
lost (an operator kill) as well as a live one, so a claim that lapsed with
nothing admitted is asked again under the next attempt, and past the attempt
ceiling stalls typed instead. A waiter on an input attaches to the first
attempt's drive and follows the ask: when the drive it waits on ends with the
input unadmitted and the row claimed under a later attempt, it attaches to
that attempt's drive. There is no repair scan for ingress. A native wake asks
for its batch's first attempt once and leaves the rest to the relay.

**ProcessStart.** The obligation id is `process_start:{process_id}`, derived
from the key the way the ingress ids are. The registering transaction's
own-commit attempt delivers through `deliver_now`; a journaled engine that
must carry its own submission context (Restate's start `send`, with its
execution context and causal invocation) claims the row itself, sends, and
settles through the same ledger. The relay's due pass is the fallback for a
lost immediate attempt; the row is delivered once and nothing re-arms it.
The engine's cancellation of that journaled claimant (its call's group
decided the call's cancel) never ends a registered start short of its send
(FIG-4127, FIG-4128): a step whose answer the cancellation took runs again,
a claim whose token it took stays for the relay while the send goes on, and
a cancellation at the send's own await proves nothing refused, so it is no
`StartFailed` compensation. A `StartFailed` request that finds a run already
holding the row goes to that run's `cancel` handler.

**Parks.** The parks arm no longer resumes a paused drive blindly: a pause
after the engine's own retries is recorded as a turn park, and only a redrive
verb resumes it. A drive paused in its admission is parked on the root the
session's next admission names (an unfinished root, an owed follow-on,
else the head input unless a command precedes it); a session already parked
keeps its park, and its verb resumes the drive with the root. A drive whose
every attempt was refused only because the park named a redrive that had not
settled (D15) waited on that redrive, not on an operator: once the redrive
settles — its park still held, or already cleared by its root's commit — the
pass resumes the drive rather than parking it again. A drive whose next work
names no root (only queued commands, a closing session's in-flight root, or
nothing) has no park a verb could resume, so the pass kills it: what the
session still holds keeps its own ingress obligation, whose relay asks for a
fresh drive, and the command lane drives the session. One whose session is
gone is killed.

**Repair.** The leader keeps one rate-bounded repair pass per kind that
reports, and arms, a row that should owe an obligation and does not. It never
finds work in the steady state.

An operator can kill an admitted `LashTurn` workflow after its ingress
obligation was delivered but before the root became terminal. Restate retries
deployment and worker failures; an explicit kill ends the workflow key for
good. The existing engine-owned lost-run pass (D23) also reads non-terminal
session roots in bounded `(session, root)` pages and checks their Restate run
status. It leaves active and paused runs alone. A run that finished failed
without a Lash outcome ends the root `SubstrateLost` (cancelled when a durable
cancel request exists), settles its bound ingress in the same transaction,
and arms the existing `ScopeClose` obligation. No new obligation kind or
host-driven recovery path is introduced (D26, FIG-3942).

## 4. Two-phase session delete

Delete writes the `CloseSession` intent, marks the session closing and
refuses new sends typed (`SessionClosing`), in one transaction. The intent's
obligation kills the running turn, cancels the session's processes and closes
its scopes. Its delivered settle arms the session's `SessionDelete`
obligation, whose deliver refuses retryably while any scope-close or
parent-end obligation of the session is undelivered, then retires the journal
and deletes the storage. The storage delete is last because it removes the
`session_meta` row the obligation lives on: every step before it is
idempotent, so a failed attempt leaves the obligation owed and the relay's
next attempt runs them all again. The ADR 0108 §5a tombstone is kept. A
stalled cleanup surfaces like any other stall.

The close's delivered settle is the transaction that writes its
`CloseSession` intent `Acknowledged`: the store arms `SessionDelete` there,
whichever path acknowledged it.

The delete does not wait for the engine's work of the session. A drive or a
root the crash interrupted may replay after the delete, and it cannot open the
session then. It still issues the steps its journal holds: each admission is
the recorded `AdmitDrive` step, and a root issues its start marker and its
`SealDriveAdmission` step. A step that ran before the delete replays its
recorded answer; one that runs after it records the retirement. A root the
close ended was sealed, and the close's engine half killed its execution
before the acknowledgement armed the delete, so no sealed root replays
against a deleted session.

A turn whose final commit the close cuts short may already have pinned its
cancel closure. Nothing drains that pin: a pin is drained at the session's
next activation, and a closing session is never activated again. The
physical delete therefore retires a closing session's pins with its storage,
and refuses a pin only on a session that is not closing. For the same reason
the delete's pre-close refusals (a pinned closure, an effect group still
live) are asked only before the close commits: a deletion retried after it
replays the recorded close step, and a refusal there would answer the replay
differently from the run that recorded it.

An input admitted before the close does not finish. The close ends its root
(`Cancelled`, cause `SessionDeleted`), and the delete retires the binding with
the session's storage. A close whose obligation stalls arms no delete, so the
unfinished root stays until an operator re-arms the close: the store counts
that session as held by the stalled close
(`UnsettledTurnCounts::held_by_stalled_close`), a
typed stall like a park. The only close there is, is a delete. The crash
matrix's control-intent cells therefore do not exclude a closing session's
in-flight root: the ingress invariant reads it until the session is deleted.

## 5. `available_at_ms` is deleted

The field, its builder, the reconcile filter and `idx_queued_work_ready` go.
The only delayed work in lash is retry backoff, which lives in
`obligation_due_at_ms`.

## 6. ADR 0108 §5 amendment

Replace the recovery paragraph of §5 with:

> A crash between the terminal commit and the close leaves the evidence
> durable and the scope open. The root's terminal transaction arms the root
> row's scope-close obligation (ADR 0109), and the close's own transaction
> delivers it. An engine that redelivers the execution replays the root to
> its recorded close step; otherwise the relay takes the due obligation and
> closes it. A delivered close is never attempted again, and no pass rescans
> terminal roots.

The conformance law in `root_terminal.rs` that asserts a second tick
"closed" the root again is rewritten to assert it closes nothing.

## 7. Fencing before the SQL lease goes

*(FIG-3927: the lease is gone (FIG-3862) and the command lane takes no
binding. The drive applies the leading command run in its own recorded step,
`drive-commands:{admission}`, under its drive fence, and the commit that
applies the run settles those rows; a row withdrawn in between refuses that
commit. The settlement waiter reads the rows' outcome and drains nothing. The
drain-and-lease arrangement below records the interim this replaced. See ADR
0101's FIG-3927 amendment.)*

The settlement waiter's command drain (`session_api.rs`, the
`try_acquire_for_executor` … `drain_next_session_command` loop) runs under
the SQL session-execution lease, not a drive epoch. Before any PR removes that
lease, the drain is fenced by the drive epoch and a test races it against a
live engine drive.

S8-I fences it by the session's current drive fence, read after the lane is
taken: a drive that seals an admission after that read refuses the drain's
commit, and the drain stops for the drive (law
`a_settlement_drain_is_refused_by_a_drive_that_seals_after_its_fence`, on the
Restate double, SQLite and PostgreSQL). The drain never raises the epoch
itself: a raise supersedes a drive mid-admission, and a superseded drive
stops, leaving what it was asked for to nobody now that no arm re-asks. A
drain that retries after a refusal presents the fence then current, so it
shares that fence with the drive that sealed it; mutual exclusion with that
drive is still the lease's. Removing the lease needs the drain to seal an
admission of its own, and to ask for a hand-over drive before it does.

## 8. Slice plan

1. **S8-F foundation**: §1's vocabulary, generic SQL, the columns on every
   §3 table (no producer writes them), the lease and its wiring, surfacing,
   and the lease and relay laws.
2. Stacked on S8-F, in parallel, each deleting the arm it replaces and
   asserting its §1.8 bound in lash-sim: **S8-I** ingress (incl. the parks
   change, `session_work_in_flight` and `TurnStatus::Stalled`); **S8-C**
   intents (incl. the child-cancel wedge and claim fencing); **S8-P**
   parent-end plans (incl. undecodable and head-of-line); **S8-S** scope close
   (incl. §6); **S8-D** two-phase delete; **S8-T** process terminal publication
   (S-14 waiters, paused terminal segments).
3. **S8-A** `available_at_ms` deletion, independent of S8-F.

The obligation layer is engine-neutral kernel code, but S8 builds no
native-specific recovery: the native in-process engine gets only the minimal
`deliver` its tests need to compile and pass, and FIG-3668 deletes it after
S8. Laws may target the Restate double first; native-only recovery tests are
not ported forward.

## Amendment (FIG-4125, 2026-09-29)

Item 14: `ArtifactCleanup` is an obligation kind in the landed vocabulary.
[ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md) and
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md) amend
cleanup and compatibility. Generation drain is not deployment drain; this ADR's
status reflects implemented obligation lanes, while 1.0 compatibility work
remains in ADR 0115.
