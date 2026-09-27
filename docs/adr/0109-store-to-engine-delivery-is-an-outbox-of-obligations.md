# 0109: Store→engine delivery is an outbox of obligations

## Status

Accepted 2026-09-27 (FIG-3600 S8). It records Sam's S8 rulings of that date.
**Not yet implemented** beyond the foundation (§1's vocabulary, relay loop and
leader lease). The per-ledger slices under *Slice plan* build the rest, and
each slice updates this status when it lands.

Amends [ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
O2 and O3 (the mechanism behind "reconcile every unacknowledged intent" and
"retry needs an explicit attempt policy") and
[ADR 0108](0108-a-process-lives-until-a-scope-its-start-could-reach.md) §5
(amendment text in §6 below). [ADR 0080](0080-substrate-attestation-is-not-a-lease-short-circuit.md)
stands: the leader lease is load control, never a fence.

## 1. Interface (frozen; slices build against this)

### 1.1 Obligation columns

A ledger row that owes the engine an effect carries these columns. The row
is the obligation: no side table. The same names are used on every ledger.

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

One CHECK, `ck_<table>_obligation`, pins the combinations:

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

Three indexes per ledger (`lash_` prefix on PostgreSQL):

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
`ParentEnd`, `SessionDelete`, `ProcessTerminal`. Its label (`ingress`,
`control_intent`, `scope_close`, `parent_end`, `session_delete`,
`process_terminal`) is the metric label and the drain-status key.

`ObligationKey` names the row an obligation lives on, one variant per kind:
`Ingress { session_id, item_id }`, `ControlIntent { intent_id }`,
`ScopeClose { session_id, root }`, `ParentEnd { parent_kind, parent_id }`,
`SessionDelete { session_id }`, `ProcessTerminal { process_id }`.

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
id), through the store's `arm_obligation` helper on that transaction.

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

The reconcile tick holds `&[Arc<dyn ObligationRelay>]`; a slice adds its
relay to the facade's list.

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

### 1.7 Duties

| Duty | Who |
|---|---|
| Restate handlers; a deployment's own process executions | every deployment |
| Immediate delivery of the deployment's own commits (`deliver_now`) | every deployment |
| Due-obligation claims (`relay_due`) | every deployment on PostgreSQL (`SKIP LOCKED`); the leader on SQLite |
| Parks arm; rate-bounded repair scans; drain hand-over (FIG-3799); park-feed compaction; opt-in evidence retention | leader only |

Every duty stays idempotent under two overlapping leaders.

### 1.8 Detection bounds the sim asserts

With tick `T` = 10 s ±10%, TTL 15 s, follower retry 5.5 s:

- **Immediate.** A producer's obligation is attempted before its call returns.
- **Lost immediate attempt.** Claimed by `due_at + T` on PostgreSQL, by
  `due_at + T + 20.5 s` on SQLite across a leader failover.
- **Lapsed claim.** Retaken by `claimed_at + claim_ttl + T`.
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
| `Ingress` | `session_ingress` | the admission transaction | the engine accepted drive request `ingress:{obligation_id}` | the drives arm and `session_work_in_flight` (deleted) |
| `ControlIntent` | `control_intents` | the verb's transaction | the engine half is applied, the intent settled, and its follow-on drive `intent:{id}` accepted | the intents arm; the `attempts` column (use `obligation_attempts`) |
| `ScopeClose` | `session_roots` | the root's terminal transaction | the close's own transaction | the scopes arm |
| `ParentEnd` | `parent_end_plans` | the plan's record | every child's cancel delivered or refused | the parent-end slot and the native worker sweep |
| `SessionDelete` | `session_meta` | the close intent's delivered settle | physical delete ran | caller-retried deletion |
| `ProcessTerminal` | `processes` | the terminal transaction | the engine's terminal promise resolved | nothing (S-14 had no owner) |

At the ceiling an intent stalls and is written `Failed{retryable: false}`,
which unwedges admission; re-arm reopens it. A parent-end plan records each
child that refused, so one unreachable child stalls its plan, never the rows
behind it. On SQLite `parent_end_plans` and `processes` live in the registry
file: they are armed in that file's transaction, not the catalog's.

**Parks.** The parks arm no longer resumes a paused drive blindly: a pause
after the engine's own retries is recorded as a turn park, and only a redrive
verb resumes it.

**Repair.** The leader keeps one rate-bounded repair pass per kind that
reports, and arms, a row that should owe an obligation and does not. It never
finds work in the steady state.

## 4. Two-phase session delete

Delete writes the `CloseSession` intent, marks the session closing and
refuses new sends typed (`SessionClosing`), in one transaction. The intent's
obligation kills the running turn, cancels the session's processes and closes
its scopes. Its delivered settle arms the session's `SessionDelete`
obligation, whose deliver refuses retryably while any scope-close or
parent-end obligation of the session is undelivered, then deletes the storage
and retires the journal. The ADR 0108 §5a tombstone is kept. A stalled
cleanup surfaces like any other stall.

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

The settlement waiter's command drain (`session_api.rs`, the
`try_acquire_for_executor` … `drain_next_session_command` loop) runs under
the SQL session-execution lease, not a drive epoch. Before any PR removes that
lease, the drain is fenced by the drive epoch and a test races it against a
live engine drive.

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
