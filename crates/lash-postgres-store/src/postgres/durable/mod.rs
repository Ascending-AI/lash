//! The durability engine over PostgreSQL: [`PostgresDurableStore`] and the
//! statements that fork from the neutral set in `lash_store_sql::durable`.
//!
//! This is the PostgreSQL engine module: apart from `schema.sql` and the
//! artifacts generated from it, no other file in this crate may name the
//! engine's tables (`scripts/check-durable-sql.py`). Each domain the runtime
//! lanes add to the fenced commit has one submodule here, owned by its lane;
//! this module dispatches each [`DomainWrite`] and [`MailDomainWrite`] to it
//! and delegates each [`DurableReads`] read to it.
//!
//! Every write is one guarded transaction: the writer fence first, then the
//! database clock read once, then the engine's statements. A claim locks its
//! node row `FOR SHARE` and its actors `FOR UPDATE SKIP LOCKED`, so a
//! concurrent reap waits for it and concurrent claimers never take one actor
//! twice; an owner commit's fence is a conditional `UPDATE` of the actor row,
//! which a concurrent claim or reap either precedes or waits behind.
//!
//! Every engine transaction also bounds itself (L8, FIG-5178): it sets
//! transaction-local lock, statement and idle-in-transaction timeouts in the
//! same round trip that reads the clock, so a convoy or a stalled client
//! surfaces as a retryable refusal instead of holding actor rows. The node
//! lease's commits and every terminal and cancel commit
//! ([`CommitLabel::RESERVED`]) run on a small pool of their own, so a burst of
//! ordinary commits cannot starve a heartbeat into a self-stop.
//!
//! Cross-node signals ([`PostgresSignals`]) live beside this module: wake
//! hints published with `pg_notify` after commit, never inside a writing
//! transaction, and each node's liveness lock, a session advisory lock its
//! listener holds.

use std::sync::{Arc, LazyLock, OnceLock};

use lash_durable::domain::{
    ExecKey, OwnerKey, ParkEventRow, ParkEventSeq, ProcessActorRow, RunRecordRow, ScopeKey,
    SessionCloseRow, SnapshotRow, TurnRow, WaitId, WaitRow,
};
use lash_durable::{
    ActorCommit, ActorKey, ActorKind, ActorSnapshot, ActorState, ActorTx, BootId, ClaimCause,
    Claimed, CommitLabel, DomainWrite, DurableError, DurableInstant, DurableReads, DurableStore,
    Epoch, Fenced, FormatSet, HeartbeatOutcome, Mail, MailAnswer, MailCommit, MailDomainWrite,
    MailKind, MailRefusal, MailSeq, MailTx, MailWrite, NodeId, NodeLease, NodeSpec, OpenedActor,
    Owner, Reaped, Release, StateRevision, StoreFailure, StoreFailureKind, Woken,
};
use lash_store_sql::Dialect;
use lash_store_sql::durable::park_events::ParkEventStatements;
use lash_store_sql::durable::processes::{ActorParkStatements, ProcessActorStatements};
use lash_store_sql::durable::{ActorStatements, MailStatements, NodeStatements};
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgConnection, PgPool, Row};

use crate::StoreError;
use crate::guarded_tx::{GuardedTx, WriterFence, begin_guarded};
use crate::support::store_sqlx_error;

mod park_events;
pub(crate) mod processes;
mod run_records;
mod session_close;
mod snapshots;
mod turns;
mod waits;

#[path = "../durable_signals.rs"]
mod signals;

pub use signals::PostgresSignals;

/// Connections held back for [`CommitLabel::RESERVED`] commits and the
/// liveness probes. The lease's commits are serial per node, so this bounds
/// how many terminal and cancel commits run at once beside them.
const RESERVED_CONNECTIONS: u32 = 4;

/// `lock_timeout` of every engine transaction, in milliseconds. S2
/// (FIG-5167) measured row-lock waits in single milliseconds at sixteen
/// nodes; a wait this long is a convoy, refused as contended.
const LOCK_TIMEOUT_MS: &str = "2000";

/// `statement_timeout` of every engine transaction, in milliseconds.
const STATEMENT_TIMEOUT_MS: &str = "5000";

/// `idle_in_transaction_session_timeout` of every engine transaction, in
/// milliseconds: a client that stalls inside a transaction loses its session
/// rather than holding actor rows.
const IDLE_IN_TRANSACTION_TIMEOUT_MS: &str = "5000";

/// The reserved pool, opened on first use over the shared pool's connect
/// options and shared by every handle of one storage.
#[derive(Clone, Default)]
pub(crate) struct Reserve(Arc<OnceLock<PgPool>>);

impl Reserve {
    fn pool(&self, shared: &PgPool) -> &PgPool {
        self.0.get_or_init(|| {
            PgPoolOptions::new()
                .max_connections(RESERVED_CONNECTIONS)
                .min_connections(0)
                .connect_lazy_with((*shared.connect_options()).clone())
        })
    }
}

/// The owner commit a domain write is applied in: after its fence.
pub(crate) struct Committing<'a> {
    /// The actor whose fence matched.
    pub(crate) actor: &'a ActorKey,
    /// The epoch it matched at; domain rows record it as `written_epoch`.
    pub(crate) epoch: Epoch,
    /// The transaction's one clock reading.
    pub(crate) now: DurableInstant,
    /// The fleet format the transaction's fence read: registry rows a
    /// domain write touches are encoded under it.
    pub(crate) fleet: lash_core_execution::FleetFormat,
}

lash_store_sql::statements! {
    /// The engine statements only PostgreSQL issues.
    pub(crate) struct PostgresDurableStatements @ "durable_postgres" {
        /// Boot `?2` of node `?1`'s lease, locked against a concurrent reap
        /// for the rest of the transaction.
        node_live = "SELECT 1 FROM nodes WHERE node_id = ?1 AND boot_id = ?2 FOR SHARE";

        /// Give up to `?5` actors claimable at `?1` in a format set of `?4`
        /// to boot `?3` of node `?2`, oldest first, bumping each epoch;
        /// returns each with the state that made it claimable. A claim that
        /// finds no commit since the previous claim counts one more failed
        /// activation; any commit since resets the count.
        claim = "WITH c AS (
                 SELECT actor_key, state FROM actors
                 WHERE ((state = 'ready' AND ready_at_ms <= ?1)
                     OR (state = 'waiting' AND next_due_ms <= ?1))
                   AND formats = ANY(?4)
                 ORDER BY COALESCE(ready_at_ms, next_due_ms), actor_key
                 LIMIT ?5
                 FOR UPDATE SKIP LOCKED
             )
             UPDATE actors AS a
             SET state = 'owned', epoch = a.epoch + 1, owner_node = ?2, owner_boot = ?3,
                 ready_at_ms = NULL, next_due_ms = NULL,
                 failed_activations = CASE WHEN a.claimed_revision = a.state_revision
                                           THEN a.failed_activations + 1 ELSE 0 END,
                 claimed_revision = a.state_revision
             FROM c
             WHERE a.actor_key = c.actor_key
             RETURNING a.actor_key, a.epoch, c.state";

        /// Lock the rows of actors `?1` in key order: a mailbox commit
        /// that wakes several actors takes their locks before any write,
        /// so two such commits never wait on each other in a cycle.
        lock_actors = "SELECT actor_key FROM actors
             WHERE actor_key = ANY(?1)
             ORDER BY actor_key
             FOR NO KEY UPDATE";

        /// Bound this transaction: lock timeout `?1`, statement timeout
        /// `?2` and idle-in-transaction timeout `?3`, all milliseconds,
        /// then the server's instant in epoch milliseconds, in one round
        /// trip.
        begin_bounded = "SELECT set_config('lock_timeout', ?1, true),
                    set_config('statement_timeout', ?2, true),
                    set_config('idle_in_transaction_session_timeout', ?3, true),
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT";

        /// Every registered boot, and whether some session holds its
        /// liveness lock. A free lock is taken for this statement's
        /// transaction only, so the probe holds nothing after it.
        liveness = "SELECT node_id, boot_id,
                    NOT pg_try_advisory_xact_lock(1818325864, hashtext(boot_id))
             FROM nodes
             ORDER BY node_id, boot_id";

        /// Take boot `?1`'s liveness lock for the rest of this transaction
        /// when no session holds it: true when it was free.
        take_liveness = "SELECT pg_try_advisory_xact_lock(1818325864, hashtext(?1))";

        /// Hold boot `?1`'s liveness lock for this session's life, waiting
        /// for an earlier session of the same boot to end.
        hold_liveness = "SELECT pg_advisory_lock(1818325864, hashtext(?1))";

        /// Send notification payload `?2[i]` on channel `?1[i]`, for each
        /// `i`, outside any writing transaction.
        notify = "SELECT pg_notify(t.channel, t.payload)
             FROM unnest(CAST(?1 AS TEXT[]), CAST(?2 AS TEXT[])) AS t(channel, payload)";
    }
}

struct Sql {
    node: NodeStatements,
    actor: ActorStatements,
    mail: MailStatements,
    park: ActorParkStatements,
    process: ProcessActorStatements,
    park_events: ParkEventStatements,
    postgres: PostgresDurableStatements,
}

static SQL: LazyLock<Sql> = LazyLock::new(|| {
    let dialect = Dialect::postgres();
    Sql {
        node: NodeStatements::render(dialect),
        actor: ActorStatements::render(dialect),
        mail: MailStatements::render(dialect),
        park: ActorParkStatements::render(dialect),
        process: ProcessActorStatements::render(dialect),
        park_events: ParkEventStatements::render(dialect),
        postgres: PostgresDurableStatements::render(dialect),
    }
});

/// The [`DurableStore`] over one PostgreSQL catalog.
#[derive(Clone)]
pub struct PostgresDurableStore {
    pool: PgPool,
    reserve: Reserve,
    fence: WriterFence,
    /// The clock a test stands in for the database's.
    #[cfg(any(test, feature = "testing"))]
    clock: Option<std::sync::Arc<dyn lash_core_execution::Clock>>,
}

impl std::fmt::Debug for PostgresDurableStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresDurableStore")
            .finish_non_exhaustive()
    }
}

type Tx = GuardedTx<'static>;

fn store_failure(error: StoreError) -> DurableError {
    let kind = match &error {
        StoreError::Contended => StoreFailureKind::Contended,
        StoreError::WriterFenced { .. } => StoreFailureKind::WriterRetired,
        _ => StoreFailureKind::Unavailable,
    };
    DurableError::Store(StoreFailure {
        kind,
        message: error.to_string(),
    })
}

fn sqlx_failure(error: sqlx::Error) -> DurableError {
    match error {
        sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_) => {
            DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Corrupt,
                message: error.to_string(),
            })
        }
        error => store_failure(store_sqlx_error(error)),
    }
}

fn corrupt(what: &str, value: &str) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message: format!("stored {what} `{value}` does not decode"),
    })
}

fn actor_key(stored: &str) -> Result<ActorKey, DurableError> {
    ActorKey::parse(stored).map_err(|_| corrupt("actor key", stored))
}

fn actor_state(stored: &str) -> Result<ActorState, DurableError> {
    ActorState::parse(stored).ok_or_else(|| corrupt("actor state", stored))
}

fn owner(node: Option<String>, boot: Option<String>) -> Option<Owner> {
    Some(Owner {
        node: NodeId::new(node?),
        boot: BootId::new(boot?),
    })
}

fn get<'r, T>(row: &'r PgRow, index: usize) -> Result<T, DurableError>
where
    T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get(index).map_err(sqlx_failure)
}

/// Commit `tx` when `outcome` is a value, roll it back when it is a refusal.
async fn finish<T>(tx: Tx, outcome: Result<T, DurableError>) -> Result<T, DurableError> {
    match outcome {
        Ok(value) => {
            tx.commit().await.map_err(sqlx_failure)?;
            Ok(value)
        }
        Err(error) => {
            tx.rollback().await.map_err(sqlx_failure)?;
            Err(error)
        }
    }
}

impl PostgresDurableStore {
    pub(crate) fn new(pool: PgPool, fence: WriterFence, reserve: Reserve) -> Self {
        Self {
            pool,
            reserve,
            fence,
            #[cfg(any(test, feature = "testing"))]
            clock: None,
        }
    }

    /// Read `clock` where the database clock would be read: a test moves
    /// time instead of waiting for it.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_clock_for_testing(
        mut self,
        clock: std::sync::Arc<dyn lash_core_execution::Clock>,
    ) -> Self {
        self.clock = Some(clock);
        self
    }

    fn injected_instant(&self) -> Option<DurableInstant> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(clock) = &self.clock {
            return Some(DurableInstant(
                i64::try_from(clock.timestamp_ms()).unwrap_or(i64::MAX),
            ));
        }
        None
    }

    async fn instant_on(
        &self,
        connection: &mut PgConnection,
    ) -> Result<DurableInstant, DurableError> {
        if let Some(now) = self.injected_instant() {
            return Ok(now);
        }
        let now: i64 = sqlx::query_scalar(
            crate::connection_sql::connection_sql()
                .select_statement_epoch_ms
                .sql(),
        )
        .fetch_one(connection)
        .await
        .map_err(sqlx_failure)?;
        Ok(DurableInstant(now))
    }

    /// A guarded, bounded transaction and the instant it runs at, on the
    /// reserved pool when `label` is reserved.
    async fn open(&self, label: CommitLabel) -> Result<(Tx, DurableInstant), DurableError> {
        tracing::trace!(label = label.as_str(), "durable postgres commit");
        let pool = if label.is_reserved() {
            self.reserve.pool(&self.pool)
        } else {
            &self.pool
        };
        let mut tx: Tx = begin_guarded(pool, &self.fence)
            .await
            .map_err(store_failure)?;
        let row = sqlx::query(SQL.postgres.begin_bounded.sql())
            .bind(LOCK_TIMEOUT_MS)
            .bind(STATEMENT_TIMEOUT_MS)
            .bind(IDLE_IN_TRANSACTION_TIMEOUT_MS)
            .fetch_one(&mut **tx)
            .await
            .map_err(sqlx_failure)?;
        let now = match self.injected_instant() {
            Some(now) => now,
            None => DurableInstant(get(&row, 3)?),
        };
        Ok((tx, now))
    }

    /// Every registered boot's liveness lock, read on the reserved pool.
    async fn liveness(&self) -> Result<Vec<lash_durable::BootLiveness>, DurableError> {
        let rows: Vec<(String, String, bool)> = sqlx::query_as(SQL.postgres.liveness.sql())
            .fetch_all(self.reserve.pool(&self.pool))
            .await
            .map_err(sqlx_failure)?;
        Ok(rows
            .into_iter()
            .map(|(node, boot, held)| lash_durable::BootLiveness {
                boot: Owner {
                    node: NodeId::new(node),
                    boot: BootId::new(boot),
                },
                held,
            })
            .collect())
    }

    /// Reap `boot` when its liveness lock is free and the reaper's own is
    /// held, in one transaction under the reap's label. Taking the free lock
    /// for the transaction keeps the boot from re-locking until the reap
    /// commits; a boot that re-locks after it finds its lease gone.
    async fn reap_released(
        &self,
        reaper: &NodeLease,
        boot: &Owner,
    ) -> Result<Vec<Reaped>, DurableError> {
        let (mut tx, now) = self.open(CommitLabel::REAP).await?;
        let outcome = async {
            if !node_live(&mut tx, &reaper.owner).await? {
                return Err(DurableError::NodeLeaseLost {
                    node: reaper.owner.node.clone(),
                });
            }
            for (owner, free) in [(&reaper.owner, false), (boot, true)] {
                let taken: bool = sqlx::query_scalar(SQL.postgres.take_liveness.sql())
                    .bind(owner.boot.as_str())
                    .fetch_one(&mut **tx)
                    .await
                    .map_err(sqlx_failure)?;
                if taken != free {
                    return Ok(Vec::new());
                }
            }
            let deleted = sqlx::query(SQL.node.delete_boot.sql())
                .bind(boot.node.as_str())
                .bind(boot.boot.as_str())
                .fetch_optional(&mut **tx)
                .await
                .map_err(sqlx_failure)?;
            if deleted.is_none() {
                return Ok(Vec::new());
            }
            Ok(release_owned_by(&mut tx, boot, now)
                .await?
                .into_iter()
                .map(|(actor, epoch)| Reaped {
                    actor,
                    from: boot.clone(),
                    epoch,
                })
                .collect())
        }
        .await;
        finish(tx, outcome).await
    }
}

async fn node_live(tx: &mut PgConnection, node: &Owner) -> Result<bool, DurableError> {
    Ok(sqlx::query(SQL.postgres.node_live.sql())
        .bind(node.node.as_str())
        .bind(node.boot.as_str())
        .fetch_optional(tx)
        .await
        .map_err(sqlx_failure)?
        .is_some())
}

async fn release_owned_by(
    tx: &mut PgConnection,
    owner: &Owner,
    now: DurableInstant,
) -> Result<Vec<(ActorKey, Epoch)>, DurableError> {
    let rows = sqlx::query(SQL.actor.release_owned_by.sql())
        .bind(owner.node.as_str())
        .bind(owner.boot.as_str())
        .bind(now.0)
        .fetch_all(tx)
        .await
        .map_err(sqlx_failure)?;
    rows.iter()
        .map(|row| Ok((actor_key(&get::<String>(row, 0)?)?, Epoch(get(row, 1)?))))
        .collect()
}

async fn fenced(
    tx: &mut PgConnection,
    actor: &ActorKey,
    held: Epoch,
) -> Result<DurableError, DurableError> {
    let current: Option<i64> = sqlx::query_scalar(SQL.actor.epoch_of.sql())
        .bind(actor.as_str())
        .fetch_optional(tx)
        .await
        .map_err(sqlx_failure)?;
    Ok(DurableError::OwnershipLost(Fenced {
        actor: actor.clone(),
        held,
        current: current.map(Epoch),
    }))
}

async fn apply_owner(
    tx: &mut Tx,
    write: &ActorTx,
    now: DurableInstant,
    fleet: lash_core_execution::FleetFormat,
) -> Result<ActorCommit, DurableError> {
    let actor = write.actor().as_str();
    let fence: Option<i64> = sqlx::query_scalar(SQL.actor.fence.sql())
        .bind(actor)
        .bind(write.epoch().0)
        .fetch_optional(&mut ***tx)
        .await
        .map_err(sqlx_failure)?;
    let Some(revision) = fence else {
        return Err(fenced(tx, write.actor(), write.epoch()).await?);
    };
    let committing = Committing {
        actor: write.actor(),
        epoch: write.epoch(),
        now,
        fleet,
    };
    for domain in write.domain() {
        apply_domain(tx, &committing, domain).await?;
    }
    if let Some(through) = write.ack() {
        for statement in [&SQL.actor.ack, &SQL.mail.delete_through] {
            sqlx::query(statement.sql())
                .bind(actor)
                .bind(through.0)
                .execute(&mut ***tx)
                .await
                .map_err(sqlx_failure)?;
        }
    }
    let state = match write.release() {
        None => ActorState::Owned,
        Some(Release::Parked) => {
            // A cancel that arrived since the owner's read is not lost to the
            // park: the actor goes ready instead, its park kept, and its
            // claimer ends it engine-free.
            let cancel_pending = sqlx::query(SQL.park.pending_mail_of_kind.sql())
                .bind(actor)
                .bind(lash_durable::domain::CANCEL_MAIL)
                .fetch_optional(&mut ***tx)
                .await
                .map_err(sqlx_failure)?
                .is_some();
            let stored: String = if cancel_pending {
                sqlx::query_scalar(SQL.actor.release.sql())
                    .bind(actor)
                    .bind("idle")
                    .bind(Option::<i64>::None)
                    .bind(now.0)
                    .fetch_one(&mut ***tx)
                    .await
                    .map_err(sqlx_failure)?
            } else {
                sqlx::query_scalar(SQL.park.park.sql())
                    .bind(actor)
                    .fetch_one(&mut ***tx)
                    .await
                    .map_err(sqlx_failure)?
            };
            actor_state(&stored)?
        }
        Some(Release::Terminal) => {
            sqlx::query(SQL.mail.delete_all.sql())
                .bind(actor)
                .execute(&mut ***tx)
                .await
                .map_err(sqlx_failure)?;
            let stored: String = sqlx::query_scalar(SQL.actor.end.sql())
                .bind(actor)
                .fetch_one(&mut ***tx)
                .await
                .map_err(sqlx_failure)?;
            actor_state(&stored)?
        }
        Some(rest) => {
            let (state, due) = match rest {
                Release::Waiting { next_due } => ("waiting", next_due.map(|due| due.0)),
                _ => ("idle", None),
            };
            let stored: String = sqlx::query_scalar(SQL.actor.release.sql())
                .bind(actor)
                .bind(state)
                .bind(due)
                .bind(now.0)
                .fetch_one(&mut ***tx)
                .await
                .map_err(sqlx_failure)?;
            actor_state(&stored)?
        }
    };
    Ok(ActorCommit {
        revision: StateRevision(revision),
        state,
    })
}

/// Wake `actor` inside the caller's transaction, reusing the port's one wake
/// statement: it takes the actor's next mailbox position and readies it when
/// it was idle or waiting. Returns the woken actor and the position taken.
///
/// Every producer transaction that writes work for an actor (pending
/// inputs, queued work, control intents, turn cancel requests, process
/// registration, trigger occurrences) calls this in its own transaction, so
/// the work and the wake commit together. `control` marks a cancel or a
/// redrive: only a control wake readies a parked actor.
pub(crate) async fn wake_within(
    tx: &mut PgConnection,
    actor: &ActorKey,
    control: bool,
    now: DurableInstant,
) -> Result<(Woken, MailSeq), DurableError> {
    let statement = if control {
        SQL.park.control_wake.sql()
    } else {
        SQL.actor.wake.sql()
    };
    let woke = sqlx::query(statement)
        .bind(actor.as_str())
        .bind(now.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(sqlx_failure)?;
    let Some(row) = woke else {
        let state: Option<(i64, String)> = sqlx::query_as(SQL.actor.epoch_of.sql())
            .bind(actor.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(sqlx_failure)?;
        return Err(DurableError::MailRefused(match state {
            None => MailRefusal::UnknownActor(actor.clone()),
            Some(_) => MailRefusal::ActorTerminal(actor.clone()),
        }));
    };
    let seq: i64 = get(&row, 0)?;
    Ok((
        Woken {
            actor: actor.clone(),
            state: actor_state(&get::<String>(&row, 1)?)?,
            owner: owner(get(&row, 2)?, get(&row, 3)?),
        },
        MailSeq(seq),
    ))
}

/// Apply one owner-commit domain write by its domain's module.
async fn apply_domain(
    tx: &mut Tx,
    committing: &Committing<'_>,
    write: &DomainWrite,
) -> Result<(), DurableError> {
    match write {
        DomainWrite::Turn(write) => turns::apply(tx, committing, write).await,
        DomainWrite::SessionCommit(write) => {
            turns::apply_session_commit(tx, committing, write).await
        }
        DomainWrite::RunRecord(write) => run_records::apply(tx, committing, write).await,
        DomainWrite::Snapshot(write) => snapshots::apply(tx, committing, write).await,
        DomainWrite::Wait(write) => waits::apply(tx, committing, write).await,
        DomainWrite::Process(write) => processes::apply(tx, committing, write).await,
        DomainWrite::SessionClose(write) => session_close::apply(tx, committing, write).await,
        DomainWrite::ParkEvent(write) => park_events::apply(tx, committing, write).await,
    }
}

/// Apply one mailbox domain write by its domain's module, with its answer
/// and the actor it woke.
async fn apply_mail_domain(
    tx: &mut PgConnection,
    write: &MailDomainWrite,
    now: DurableInstant,
    fleet: lash_core_execution::FleetFormat,
) -> Result<(MailAnswer, Option<Woken>), DurableError> {
    Ok(match write {
        MailDomainWrite::ResolveWait(resolution) => {
            let (answer, woken) = waits::resolve(tx, resolution, now).await?;
            (MailAnswer::ResolveWait(answer), woken)
        }
        MailDomainWrite::RequestProcessCancel(request) => {
            let (answer, woken) = processes::request_cancel(tx, request, now, fleet).await?;
            (MailAnswer::RequestProcessCancel(answer), woken)
        }
        MailDomainWrite::RequestTurnCancel(request) => {
            let (answer, woken) = turns::request_cancel(tx, request, now).await?;
            (MailAnswer::RequestTurnCancel(answer), woken)
        }
        MailDomainWrite::Redrive(request) => {
            let (answer, woken) = park_events::redrive(tx, request, now).await?;
            (MailAnswer::Redrive(answer), woken)
        }
    })
}

fn note_woken(woken: &mut Vec<Woken>, entry: Woken) {
    match woken.iter_mut().find(|seen| seen.actor == entry.actor) {
        Some(seen) => *seen = entry,
        None => woken.push(entry),
    }
}

/// Lock, in key order, every actor `writes` appends to or wakes, when
/// there are several: rows locked in write order would let two commits that
/// name the same actors in opposite orders deadlock. A domain write keeps
/// its own order (its row, then the actor it wakes) after these.
async fn lock_mail_targets(tx: &mut PgConnection, writes: &MailTx) -> Result<(), DurableError> {
    let mut targets: Vec<&str> = writes
        .writes()
        .iter()
        .filter_map(|write| match write {
            MailWrite::Append { actor, .. } | MailWrite::Wake { actor } => Some(actor.as_str()),
            MailWrite::CreateActor { .. } | MailWrite::Domain(_) => None,
        })
        .collect();
    targets.sort_unstable();
    targets.dedup();
    if targets.len() > 1 {
        sqlx::query(SQL.postgres.lock_actors.sql())
            .bind(&targets)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_failure)?;
    }
    Ok(())
}

async fn apply_mail(
    tx: &mut PgConnection,
    writes: &MailTx,
    now: DurableInstant,
    fleet: lash_core_execution::FleetFormat,
) -> Result<MailCommit, DurableError> {
    lock_mail_targets(tx, writes).await?;
    let mut receipt = MailCommit::default();
    for write in writes.writes() {
        match write {
            MailWrite::CreateActor { actor, formats } => {
                let created = sqlx::query(SQL.actor.create.sql())
                    .bind(actor.as_str())
                    .bind(actor.kind().as_str())
                    .bind(formats.as_str())
                    .bind(now.0)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(sqlx_failure)?;
                if created.is_none() {
                    return Err(DurableError::MailRefused(MailRefusal::ActorExists(
                        actor.clone(),
                    )));
                }
                note_woken(
                    &mut receipt.woken,
                    Woken {
                        actor: actor.clone(),
                        state: ActorState::Ready,
                        owner: None,
                    },
                );
            }
            MailWrite::Append { actor, .. } | MailWrite::Wake { actor } => {
                let (woken, seq) = wake_within(tx, actor, false, now).await?;
                if let MailWrite::Append { kind, body, .. } = write {
                    sqlx::query(SQL.mail.append.sql())
                        .bind(actor.as_str())
                        .bind(seq.0)
                        .bind(kind.as_str())
                        .bind(body)
                        .bind(now.0)
                        .execute(&mut *tx)
                        .await
                        .map_err(sqlx_failure)?;
                    receipt.appended.push((actor.clone(), seq));
                }
                note_woken(&mut receipt.woken, woken);
            }
            MailWrite::Domain(domain) => {
                let (answer, woken) = apply_mail_domain(tx, domain, now, fleet).await?;
                receipt.answers.push(answer);
                if let Some(woken) = woken {
                    note_woken(&mut receipt.woken, woken);
                }
            }
        }
    }
    Ok(receipt)
}

#[async_trait::async_trait]
impl DurableStore for PostgresDurableStore {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        let mut connection = self.pool.acquire().await.map_err(sqlx_failure)?;
        self.instant_on(&mut connection).await
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        let lease_owner = Owner {
            node: spec.node.clone(),
            boot: BootId::new(uuid::Uuid::new_v4().to_string()),
        };
        let formats: Vec<&str> = spec.decodes.iter().map(FormatSet::as_str).collect();
        let formats = serde_json::Value::from(formats).to_string();
        let (mut tx, now) = self.open(CommitLabel::NODE_REGISTER).await?;
        let outcome = async {
            let boots: Vec<String> = sqlx::query_scalar(SQL.node.delete_boots.sql())
                .bind(spec.node.as_str())
                .fetch_all(&mut **tx)
                .await
                .map_err(sqlx_failure)?;
            for boot in boots {
                let earlier = Owner {
                    node: spec.node.clone(),
                    boot: BootId::new(boot),
                };
                release_owned_by(&mut tx, &earlier, now).await?;
            }
            let expires_at = now.after_millis(spec.ttl_millis);
            sqlx::query(SQL.node.insert.sql())
                .bind(lease_owner.node.as_str())
                .bind(lease_owner.boot.as_str())
                .bind(&formats)
                .bind(now.0)
                .bind(expires_at.0)
                .execute(&mut **tx)
                .await
                .map_err(sqlx_failure)?;
            Ok(NodeLease {
                owner: lease_owner,
                decodes: spec.decodes.clone(),
                ttl_millis: spec.ttl_millis,
                expires_at,
            })
        }
        .await;
        finish(tx, outcome).await
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        let (mut tx, now) = self.open(CommitLabel::HEARTBEAT).await?;
        let outcome = sqlx::query_scalar::<_, i64>(SQL.node.renew.sql())
            .bind(node.owner.node.as_str())
            .bind(node.owner.boot.as_str())
            .bind(now.after_millis(node.ttl_millis).0)
            .fetch_optional(&mut **tx)
            .await
            .map_err(sqlx_failure)
            .map(|renewed| match renewed {
                Some(expires_at) => HeartbeatOutcome::Renewed {
                    expires_at: DurableInstant(expires_at),
                },
                None => HeartbeatOutcome::Reaped,
            });
        finish(tx, outcome).await
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        let (mut tx, now) = self.open(CommitLabel::REAP).await?;
        let outcome = async {
            if !node_live(&mut tx, &reaper.owner).await? {
                return Err(DurableError::NodeLeaseLost {
                    node: reaper.owner.node.clone(),
                });
            }
            let dead: Vec<(String, String)> = sqlx::query_as(SQL.node.delete_expired.sql())
                .bind(now.0)
                .fetch_all(&mut **tx)
                .await
                .map_err(sqlx_failure)?;
            let mut reaped = Vec::new();
            for (node, boot) in dead {
                let from = Owner {
                    node: NodeId::new(node),
                    boot: BootId::new(boot),
                };
                for (actor, epoch) in release_owned_by(&mut tx, &from, now).await? {
                    reaped.push(Reaped {
                        actor,
                        from: from.clone(),
                        epoch,
                    });
                }
            }
            Ok(reaped)
        }
        .await;
        finish(tx, outcome).await
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        let (mut tx, now) = self.open(CommitLabel::NODE_RELEASE).await?;
        let outcome = async {
            let deleted = sqlx::query(SQL.node.delete_boot.sql())
                .bind(node.owner.node.as_str())
                .bind(node.owner.boot.as_str())
                .fetch_optional(&mut **tx)
                .await
                .map_err(sqlx_failure)?;
            if deleted.is_none() {
                return Err(DurableError::NodeLeaseLost {
                    node: node.owner.node.clone(),
                });
            }
            Ok(release_owned_by(&mut tx, &node.owner, now)
                .await?
                .into_iter()
                .map(|(actor, _)| actor)
                .collect())
        }
        .await;
        finish(tx, outcome).await
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        let formats: Vec<String> = node
            .decodes
            .iter()
            .map(|formats| formats.as_str().to_owned())
            .collect();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let (mut tx, now) = self.open(CommitLabel::CLAIM).await?;
        let outcome = async {
            if !node_live(&mut tx, &node.owner).await? {
                return Err(DurableError::NodeLeaseLost {
                    node: node.owner.node.clone(),
                });
            }
            let rows = sqlx::query(SQL.postgres.claim.sql())
                .bind(now.0)
                .bind(node.owner.node.as_str())
                .bind(node.owner.boot.as_str())
                .bind(&formats)
                .bind(limit)
                .fetch_all(&mut **tx)
                .await
                .map_err(sqlx_failure)?;
            let mut claimed = rows
                .iter()
                .map(|row| {
                    Ok(Claimed {
                        actor: actor_key(&get::<String>(row, 0)?)?,
                        epoch: Epoch(get(row, 1)?),
                        cause: if get::<String>(row, 2)? == ActorState::Waiting.as_str() {
                            ClaimCause::Due
                        } else {
                            ClaimCause::Ready
                        },
                    })
                })
                .collect::<Result<Vec<_>, DurableError>>()?;
            claimed.sort_by(|left, right| left.actor.cmp(&right.actor));
            Ok(claimed)
        }
        .await;
        finish(tx, outcome).await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        let rows: Vec<(String, i64)> = sqlx::query_as(SQL.actor.owned_by.sql())
            .bind(node.owner.node.as_str())
            .bind(node.owner.boot.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(sqlx_failure)?;
        rows.into_iter()
            .map(|(key, epoch)| {
                Ok(Claimed {
                    actor: actor_key(&key)?,
                    epoch: Epoch(epoch),
                    cause: ClaimCause::Adopted,
                })
            })
            .collect()
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        let rows = sqlx::query(SQL.actor.open.sql())
            .bind(actor.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(sqlx_failure)?;
        let Some(first) = rows.first() else {
            return Err(DurableError::OwnershipLost(Fenced {
                actor: actor.clone(),
                held: epoch,
                current: None,
            }));
        };
        let current = Epoch(get(first, 0)?);
        if current != epoch || get::<String>(first, 1)? != ActorState::Owned.as_str() {
            return Err(DurableError::OwnershipLost(Fenced {
                actor: actor.clone(),
                held: epoch,
                current: Some(current),
            }));
        }
        let mut opened = OpenedActor {
            actor: actor.clone(),
            epoch,
            revision: StateRevision(get(first, 2)?),
            acked: MailSeq(get(first, 3)?),
            seen: MailSeq(get(first, 4)?),
            mail: Vec::new(),
        };
        for row in &rows {
            if let Some(seq) = get::<Option<i64>>(row, 5)? {
                opened.mail.push(Mail {
                    seq: MailSeq(seq),
                    kind: MailKind::new(get::<String>(row, 6)?),
                    body: get(row, 7)?,
                    appended_at: DurableInstant(get(row, 8)?),
                });
            }
        }
        Ok(ActorTx::opened(opened))
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        if tx.ack().is_some_and(|through| through > tx.seen()) {
            return Err(DurableError::AckBeyondRead {
                actor: tx.actor().clone(),
            });
        }
        let (mut guarded, now) = self.open(label).await?;
        let outcome = apply_owner(&mut guarded, &tx, now, self.fence.fleet()).await;
        finish(guarded, outcome).await
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        let (mut guarded, now) = self.open(label).await?;
        let outcome = apply_mail(&mut guarded, &tx, now, self.fence.fleet()).await;
        finish(guarded, outcome).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        let Some(row) = sqlx::query(SQL.actor.snapshot.sql())
            .bind(actor.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(sqlx_failure)?
        else {
            return Ok(None);
        };
        let kind: String = get(&row, 0)?;
        let mail_seq: i64 = get(&row, 5)?;
        let acked_seq: i64 = get(&row, 6)?;
        Ok(Some(ActorSnapshot {
            actor: actor.clone(),
            kind: ActorKind::parse(&kind).ok_or_else(|| corrupt("actor kind", &kind))?,
            state: actor_state(&get::<String>(&row, 1)?)?,
            epoch: Epoch(get(&row, 2)?),
            owner: owner(get(&row, 3)?, get(&row, 4)?),
            has_mail: mail_seq > acked_seq,
            next_due: get::<Option<i64>>(&row, 7)?.map(DurableInstant),
            revision: StateRevision(get(&row, 8)?),
            formats: FormatSet::new(get::<String>(&row, 9)?),
            pending_mail: u64::try_from(get::<i64>(&row, 10)?).unwrap_or_default(),
            park: get(&row, 11)?,
            failed_activations: u32::try_from(get::<i64>(&row, 12)?).unwrap_or(u32::MAX),
        }))
    }
}

impl PostgresDurableStore {
    /// A pooled connection for one unfenced domain read.
    async fn reader(&self) -> Result<sqlx::pool::PoolConnection<sqlx::Postgres>, DurableError> {
        self.pool.acquire().await.map_err(sqlx_failure)
    }
}

#[async_trait::async_trait]
impl DurableReads for PostgresDurableStore {
    async fn turn(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Option<TurnRow>, DurableError> {
        turns::turn(&mut *self.reader().await?, session).await
    }

    async fn turn_end(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Option<lash_durable::domain::TurnEnd>, DurableError> {
        turns::turn_end(&mut *self.reader().await?, session, run).await
    }

    async fn run_records(&self, owner: &OwnerKey) -> Result<Vec<RunRecordRow>, DurableError> {
        run_records::read(&mut *self.reader().await?, owner).await
    }

    async fn snapshot(&self, exec: &ExecKey) -> Result<Option<SnapshotRow>, DurableError> {
        snapshots::read(&mut *self.reader().await?, exec).await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
        waits::pending(&mut *self.reader().await?, owner).await
    }

    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
        waits::wait(&mut *self.reader().await?, id).await
    }

    async fn process(
        &self,
        process: &lash_sansio::ProcessId,
    ) -> Result<Option<ProcessActorRow>, DurableError> {
        processes::process(&mut *self.reader().await?, process).await
    }

    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<lash_sansio::ProcessId>, DurableError> {
        processes::live_until_descendants(&mut *self.reader().await?, scope, limit).await
    }

    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&lash_sansio::ProcessId>,
        limit: usize,
    ) -> Result<Vec<lash_sansio::ProcessId>, DurableError> {
        processes::until_children(&mut *self.reader().await?, scope, after, limit).await
    }

    async fn session_close(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError> {
        session_close::read(&mut *self.reader().await?, session).await
    }

    async fn ending_scopes(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Vec<ScopeKey>, DurableError> {
        session_close::ending_scopes(&mut *self.reader().await?, session).await
    }

    async fn park_events(
        &self,
        after: Option<ParkEventSeq>,
        limit: usize,
    ) -> Result<Vec<ParkEventRow>, DurableError> {
        park_events::read(&mut *self.reader().await?, after, limit).await
    }
}

#[cfg(test)]
#[path = "../durable_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "../wait_law_tests.rs"]
mod wait_law_tests;

#[cfg(test)]
#[path = "../process_law_tests.rs"]
mod process_law_tests;

#[cfg(test)]
#[path = "../durable_concurrency_tests.rs"]
mod concurrency_tests;
