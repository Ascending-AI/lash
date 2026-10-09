//! The durability engine over PostgreSQL: [`PostgresDurableStore`] and the
//! statements that fork from the neutral set in `lash_store_sql::durable`.
//!
//! This is the PostgreSQL engine module: apart from `schema.sql` and the
//! artifacts generated from it, no other file in this crate may name the
//! engine's tables (`scripts/check-durable-sql.py`). Each domain the runtime
//! lanes add to the fenced commit has one submodule here, owned by its lane;
//! this module dispatches each [`DomainWrite`] and [`MailDomainWrite`] to it
//! and delegates each [`lash_durable::DurableReads`] read to it.
//!
//! Every write is one guarded transaction: the writer fence first, then the
//! database clock read once, then the engine's statements. A claim locks its
//! node row `FOR SHARE` and its actors `FOR UPDATE SKIP LOCKED`, so a
//! concurrent reap waits for it and concurrent claimers never take one actor
//! twice; an owner commit's fence is a conditional `UPDATE` of the actor row,
//! which a concurrent claim or reap either precedes or waits behind.
//!
//! Every engine transaction also bounds itself (L8, FIG-5178, FIG-5240): its
//! role's lock, statement and idle-in-transaction limits are sent with its
//! `BEGIN`, before the fence's first lock wait, so a convoy or a stalled
//! client surfaces as a retryable refusal instead of holding actor rows, and
//! the whole operation, checkout included, runs within its role's deadline.
//! Each commit runs on the capacity its label names
//! ([`CommitLabel::capacity`]): the node lease's commits on per-node renewal
//! connections, claims and drain marks on the scheduler pool, every reap,
//! release, hand-back, cancel and terminal on the critical pool, and the rest
//! on the work pool behind `max_store_operations` admission, so a burst of
//! ordinary commits cannot starve a heartbeat into a self-stop.
//!
//! Node wakes ([`PostgresNodeWakes`]) live beside this module: wake hints
//! published with `pg_notify` after commit, never inside a writing
//! transaction, and each node's liveness lock, a session advisory lock its
//! listener holds.

use std::sync::{Arc, LazyLock};

use lash_durable::{
    ActorCommit, ActorKey, ActorSnapshot, ActorState, ActorTx, BootId, ClaimCause, ClaimPurpose,
    Claimed, CommitCapacity, CommitLabel, DomainWrite, DurableError, DurableInstant, DurableStore,
    Epoch, Fenced, FormatSet, HeartbeatOutcome, Mail, MailAnswer, MailCommit, MailDomainWrite,
    MailKind, MailRefusal, MailSeq, MailTx, MailWrite, NodeId, NodeLease, NodeSpec, OpenedActor,
    Owner, Reaped, Release, StateRevision, StoreFailure, StoreFailureKind, Woken,
};
use lash_store_sql::Dialect;
use lash_store_sql::durable::park_events::ParkEventStatements;
use lash_store_sql::durable::processes::{ActorParkStatements, ProcessActorStatements};
use lash_store_sql::durable::session_mail::SessionMailStatements;
use lash_store_sql::durable::{ActorStatements, MailStatements, NodeStatements};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Row};

use crate::StoreError;
use crate::guarded_tx::{GuardedTx, WriterFence, begin_durable};
use crate::host::{RolePools, TransactionPrelude};
use crate::support::store_sqlx_error;

#[path = "../durable_admission.rs"]
mod admission;
mod park_events;
pub(crate) mod processes;
mod prompts;
pub(crate) use prompts::release as release_prompt_snapshots;
#[path = "../durable_reads.rs"]
mod reads;
#[path = "../durable_replay.rs"]
mod replay;
mod run_records;
mod session_close;
mod session_mail;
#[cfg(any(test, feature = "testing"))]
pub(crate) use session_mail::cut_session_wakes;
mod snapshots;
mod turns;
mod waits;

#[path = "../node_wakes.rs"]
mod node_wakes;

pub use node_wakes::PostgresNodeWakes;

/// The owner commit a domain write is applied in: after its fence.
pub(crate) struct Committing<'a> {
    pub(crate) checkpoint_ref_chunk: usize,
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
        /// for the rest of the transaction: its draining flag, or no row.
        node_live = "SELECT draining FROM nodes WHERE node_id = ?1 AND boot_id = ?2 FOR SHARE";

        /// Give up to `?5` actors claimable at `?1` in a format set of `?4`,
        /// or process actors in another set with mail of kind `?6` (a
        /// cancel) pending, to boot `?3` of node `?2`, oldest first, bumping
        /// each epoch; returns each with the state that made it claimable
        /// and its format set. A claim that finds no commit since the
        /// previous claim counts one more failed activation; any commit
        /// since resets the count.
        claim = "WITH c AS (
                 SELECT p.actor_key, p.state, p.formats FROM actors p
                 WHERE ((p.state = 'ready' AND p.ready_at_ms <= ?1)
                     OR (p.state = 'waiting' AND p.next_due_ms <= ?1))
                   AND (p.formats = ANY(?4)
                     OR (p.kind = 'process' AND EXISTS (
                         SELECT 1 FROM actor_mail m
                         WHERE m.actor_key = p.actor_key AND m.seq > p.acked_seq
                           AND m.kind = ?6)))
                 ORDER BY COALESCE(p.ready_at_ms, p.next_due_ms), p.actor_key
                 LIMIT ?5
                 FOR UPDATE OF p SKIP LOCKED
             )
             UPDATE actors AS a
             SET state = 'owned', epoch = a.epoch + 1, owner_node = ?2, owner_boot = ?3,
                 ready_at_ms = NULL, next_due_ms = NULL,
                 failed_activations = CASE WHEN a.claimed_revision = a.state_revision
                                           THEN a.failed_activations + 1 ELSE 0 END,
                 claimed_revision = a.state_revision
             FROM c
             WHERE a.actor_key = c.actor_key
             RETURNING a.actor_key, a.epoch, c.state, c.formats";

        /// Session `?2`'s write authority when its actor `?1` exists: the
        /// actor row, then the history lock (seed `1`), in that order, and
        /// one row. The aggregate reads the whole locking subquery before
        /// the projection takes the advisory lock, so one statement keeps
        /// the order. No row, and no lock, when the actor does not exist.
        lock_session_writes = "SELECT held.locked, pg_advisory_xact_lock(hashtextextended(?2, 1::bigint))
             FROM (
                 SELECT count(*) AS locked FROM (
                     SELECT 1 FROM actors WHERE actor_key = ?1 FOR NO KEY UPDATE
                 ) AS actor
             ) AS held
             WHERE held.locked > 0";

        /// Lock the rows of actors `?1` in key order: a mailbox commit
        /// that wakes several actors takes their locks before any write,
        /// so two such commits never wait on each other in a cycle.
        lock_actors = "SELECT actor_key FROM actors
             WHERE actor_key = ANY(?1)
             ORDER BY actor_key
             FOR NO KEY UPDATE";

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

        /// The owner's open of actor `?1`: its row, its pending mail oldest
        /// first, the database clock and, when `?2` names its session, that
        /// session's unfinished turn's accepted cancel request, in one read.
        open = "SELECT a.epoch, a.state, a.state_revision, a.acked_seq, a.mail_seq,
                    m.seq, m.kind, m.body, m.appended_at_ms,
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT,
                    c.turn_id, c.request_id, c.origin, c.reason, c.disposition, c.mode
             FROM actors a
             LEFT JOIN actor_mail m ON m.actor_key = a.actor_key AND m.seq > a.acked_seq
             LEFT JOIN session_runs r ON r.session_id = ?2 AND r.admission_json IS NOT NULL
                 AND r.terminal_kind IS NULL
             LEFT JOIN turn_phases p ON p.session_id = r.session_id AND p.run = r.run
             LEFT JOIN turn_cancel_requests c ON c.session_id = p.session_id AND c.turn_id = p.run
             WHERE a.actor_key = ?1
             ORDER BY m.seq";

        /// A transaction's first statement after its fenced `BEGIN`: the
        /// epoch the writer fence admits (no row when the fleet record is
        /// absent) and the database clock.
        fence_and_clock = "SELECT (SELECT format_version FROM fleet_format WHERE singleton = TRUE),
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT";

        /// [`Self::fence_and_clock`] and the transaction's id, by which a
        /// lost `COMMIT` is reconciled: a mailbox commit's first statement.
        fence_clock_and_xact = "SELECT (SELECT format_version FROM fleet_format WHERE singleton = TRUE),
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT,
                    pg_current_xact_id()::text";

        /// An owner commit's first statement: [`Self::fence_clock_and_xact`]
        /// and actor `?1`'s ownership fence at epoch `?2`, its state revision
        /// bumped, or no revision when the epoch is not current.
        owner_envelope = "WITH fenced AS (
                 UPDATE actors SET state_revision = state_revision + 1
                 WHERE actor_key = ?1 AND epoch = ?2 AND state = 'owned'
                 RETURNING state_revision
             )
             SELECT (SELECT format_version FROM fleet_format WHERE singleton = TRUE),
                    (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT,
                    pg_current_xact_id()::text,
                    (SELECT state_revision FROM fenced)";

        /// Acknowledge actor `?1`'s mail through `?2` and delete it, in one
        /// statement.
        ack_through = "WITH acked AS (
                 UPDATE actors SET acked_seq = ?2 WHERE actor_key = ?1 AND acked_seq < ?2
             )
             DELETE FROM actor_mail WHERE actor_key = ?1 AND seq <= ?2";

        /// Park actor `?1`, its epoch bumped, unless mail of kind `?3` (a
        /// cancel) is pending: then it is released `ready` at `?2` instead,
        /// its park kept, as a release to `idle` with mail would be. Returns
        /// the state it took.
        park_unless_pending = "WITH pending AS (
                 SELECT EXISTS (
                     SELECT 1 FROM actor_mail m, actors p
                     WHERE p.actor_key = ?1 AND m.actor_key = ?1 AND m.seq > p.acked_seq
                       AND m.kind = ?3
                 ) AS mail
             )
             UPDATE actors AS a
             SET state = CASE WHEN NOT pending.mail THEN 'parked'
                              WHEN a.mail_seq > a.acked_seq THEN 'ready'
                              ELSE 'idle' END,
                 ready_at_ms = CASE WHEN pending.mail AND a.mail_seq > a.acked_seq
                                    THEN CAST(?2 AS BIGINT) ELSE NULL END,
                 next_due_ms = NULL,
                 epoch = a.epoch + 1, owner_node = NULL, owner_boot = NULL
             FROM pending
             WHERE a.actor_key = ?1
             RETURNING a.state";

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
    session_mail: SessionMailStatements,
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
        session_mail: SessionMailStatements::render(dialect),
        postgres: PostgresDurableStatements::render(dialect),
    }
});

/// The [`DurableStore`] over one PostgreSQL catalog.
#[derive(Clone)]
pub struct PostgresDurableStore {
    pools: Arc<RolePools>,
    fence: WriterFence,
    observer: lash_core_execution::facade_support::StoreObserver,
    /// The clock a test stands in for the database's.
    #[cfg(any(test, feature = "testing"))]
    clock: Option<std::sync::Arc<dyn lash_core_execution::Clock>>,
    /// A connection a test loses at the next commit's `COMMIT`.
    #[cfg(any(test, feature = "testing"))]
    commit_fault: Option<Arc<crate::testing::CommitFault>>,
}

impl std::fmt::Debug for PostgresDurableStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresDurableStore")
            .finish_non_exhaustive()
    }
}

type Tx = GuardedTx<'static>;

/// Model a shared server's scheduling delay at registration and claim,
/// even when a claim finds no actors. The crash fixture's guard law uses
/// real statements rather than delaying before a statement's timer starts.
#[cfg(any(test, feature = "testing"))]
pub(crate) async fn delay_node_statements_for_testing(
    pool: &PgPool,
    delay: std::time::Duration,
) -> sqlx::Result<()> {
    sqlx::raw_sql(&format!(
        "CREATE FUNCTION slow_matrix_statement() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN PERFORM pg_sleep(TG_ARGV[0]::double precision); RETURN NULL; END $$;
         CREATE TRIGGER slow_registration BEFORE INSERT ON lash_nodes
             FOR EACH STATEMENT EXECUTE FUNCTION slow_matrix_statement('{seconds}');
         CREATE TRIGGER slow_claim BEFORE UPDATE ON lash_actors
             FOR EACH STATEMENT EXECUTE FUNCTION slow_matrix_statement('{seconds}');",
        seconds = delay.as_secs_f64(),
    ))
    .execute(crate::observed_sql::executor(pool))
    .await
    .map(|_| ())
}

/// An operation that did not finish within its role's deadline: whether a
/// commit in it landed is unknown.
fn deadline_failure(deadline: std::time::Duration) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Unavailable,
        message: format!(
            "durable operation did not finish within its {} ms deadline",
            deadline.as_millis()
        ),
    })
}

/// The pool and guard profile a capacity's operations run on, and whether
/// they take `max_store_operations` admission first.
struct Route<'a> {
    pool: &'a PgPool,
    prelude: &'a TransactionPrelude,
    admitted: bool,
}

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

/// Refuse an integer that the SQL or domain representation cannot carry.
fn integer<T>(value: impl TryInto<T, Error: std::fmt::Display>) -> Result<T, DurableError> {
    value
        .try_into()
        .map_err(|error| corrupt("integer", &error.to_string()))
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
    pub(crate) fn new(
        pools: Arc<RolePools>,
        fence: WriterFence,
        observer: lash_core_execution::facade_support::StoreObserver,
    ) -> Self {
        Self {
            pools,
            fence,
            observer,
            #[cfg(any(test, feature = "testing"))]
            clock: None,
            #[cfg(any(test, feature = "testing"))]
            commit_fault: None,
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

    /// Lose the connection of the next commit's `COMMIT` as `fault` says.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_commit_fault_for_testing(
        mut self,
        fault: Arc<crate::testing::CommitFault>,
    ) -> Self {
        self.commit_fault = Some(fault);
        self
    }

    fn injected_instant(&self) -> Option<Result<DurableInstant, DurableError>> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(clock) = &self.clock {
            return Some(integer(clock.timestamp_ms()).map(DurableInstant));
        }
        None
    }

    async fn instant_on(
        &self,
        connection: &mut PgConnection,
    ) -> Result<DurableInstant, DurableError> {
        if let Some(now) = self.injected_instant().transpose()? {
            return Ok(now);
        }
        let now: i64 = sqlx::query_scalar(
            crate::connection_sql::connection_sql()
                .select_statement_epoch_ms
                .sql(),
        )
        .fetch_one(crate::observed_sql::executor(connection))
        .await
        .map_err(sqlx_failure)?;
        Ok(DurableInstant(now))
    }

    /// Where `capacity`'s operations run.
    /// A guarded, bounded transaction on `label`'s capacity and the instant
    /// it runs at: the role's guards and the fence lock with `BEGIN`, then
    /// the fence's read and the database clock in one statement. Called
    /// inside [`within`](Self::within).
    async fn open(&self, label: CommitLabel) -> Result<(Tx, DurableInstant), DurableError> {
        tracing::trace!(label = label.as_str(), "durable postgres commit");
        let route = self.route(label.capacity());
        let mut locked = begin_durable(route.pool, &self.fence, route.prelude)
            .await
            .map_err(store_failure)?;
        let (recorded, now): (Option<i32>, i64) =
            sqlx::query_as(SQL.postgres.fence_and_clock.sql())
                .fetch_one(crate::observed_sql::executor(locked.connection()))
                .await
                .map_err(sqlx_failure)?;
        let tx = locked
            .admit(&self.fence, recorded)
            .await
            .map_err(store_failure)?;
        Ok((tx, self.instant_or(now)?))
    }

    /// The instant a transaction runs at: the clock a test stands in, or
    /// `now`, the database's, read with the transaction's first statement.
    fn instant_or(&self, now: i64) -> Result<DurableInstant, DurableError> {
        Ok(self
            .injected_instant()
            .transpose()?
            .unwrap_or(DurableInstant(now)))
    }

    /// Every registered boot's liveness lock, read on the scheduler pool.
    async fn liveness(&self) -> Result<Vec<lash_durable::BootLiveness>, DurableError> {
        self.within(CommitCapacity::Scheduler, async {
            let rows: Vec<(String, String, bool)> = sqlx::query_as(SQL.postgres.liveness.sql())
                .fetch_all(crate::observed_sql::executor(&self.pools.scheduler))
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
        })
        .await
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
        self.within(CommitCapacity::Critical, async {
            crate::observed_sql::measure(&self.observer, CommitLabel::REAP, 0, async {
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
                            .fetch_one(crate::observed_sql::executor(&mut **tx))
                            .await
                            .map_err(sqlx_failure)?;
                        if taken != free {
                            return Ok(Vec::new());
                        }
                    }
                    let deleted = sqlx::query(SQL.node.delete_boot.sql())
                        .bind(boot.node.as_str())
                        .bind(boot.boot.as_str())
                        .fetch_optional(crate::observed_sql::executor(&mut **tx))
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
            })
            .await
        })
        .await
    }
}

async fn node_live(tx: &mut PgConnection, node: &Owner) -> Result<bool, DurableError> {
    Ok(node_draining(tx, node).await?.is_some())
}

/// Whether `node` is draining, or `None` when it holds no lease.
async fn node_draining(tx: &mut PgConnection, node: &Owner) -> Result<Option<bool>, DurableError> {
    sqlx::query_scalar(SQL.postgres.node_live.sql())
        .bind(node.node.as_str())
        .bind(node.boot.as_str())
        .fetch_optional(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)
}

/// The format sets a node's stored `formats_json` names.
fn decoded_sets(stored: &str) -> Result<Vec<FormatSet>, DurableError> {
    let sets: Vec<String> =
        serde_json::from_str(stored).map_err(|_| corrupt("node format sets", stored))?;
    Ok(sets.into_iter().map(FormatSet::new).collect())
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
        .fetch_all(crate::observed_sql::executor(tx))
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
        .fetch_optional(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(DurableError::OwnershipLost(Fenced {
        actor: actor.clone(),
        held,
        current: current.map(Epoch),
    }))
}

fn group_members(tx: &ActorTx) -> u64 {
    tx.domain()
        .iter()
        .filter(|write| {
            matches!(
                write,
                DomainWrite::RunRecord(lash_durable::domain::RunRecordWrite::Append {
                    kind: lash_durable::domain::RunRecordKind::XOutcome,
                    ..
                })
            )
        })
        .count() as u64
}

/// Apply the owner's `write` after its ownership fence, which the commit's
/// first statement ran ([`PostgresDurableStatements::owner_envelope`]):
/// `fence` is the state revision it bumped, `None` when the epoch was not
/// current.
async fn apply_owner(
    tx: &mut Tx,
    write: &ActorTx,
    fence: Option<i64>,
    now: DurableInstant,
    fleet: lash_core_execution::FleetFormat,
    checkpoint_ref_chunk: usize,
) -> Result<ActorCommit, DurableError> {
    let actor = write.actor().as_str();
    let Some(revision) = fence else {
        return Err(fenced(tx, write.actor(), write.epoch()).await?);
    };
    let committing = Committing {
        checkpoint_ref_chunk,
        actor: write.actor(),
        epoch: write.epoch(),
        now,
        fleet,
    };
    for domain in write.domain() {
        apply_domain(tx, &committing, domain).await?;
    }
    if let Some(formats) = write.formats() {
        sqlx::query(SQL.actor.stamp_formats.sql())
            .bind(actor)
            .bind(formats.as_str())
            .execute(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(sqlx_failure)?;
    }
    if let Some(through) = write.ack() {
        sqlx::query(SQL.postgres.ack_through.sql())
            .bind(actor)
            .bind(through.0)
            .execute(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(sqlx_failure)?;
    }
    let state = match write.release() {
        None => ActorState::Owned,
        Some(Release::Parked) => {
            // A cancel that arrived since the owner's read is not lost to the
            // park: the actor goes ready instead, its park kept, and its
            // claimer ends it engine-free.
            let stored: String = sqlx::query_scalar(SQL.postgres.park_unless_pending.sql())
                .bind(actor)
                .bind(now.0)
                .bind(lash_durable::domain::CANCEL_MAIL)
                .fetch_one(crate::observed_sql::executor(&mut ***tx))
                .await
                .map_err(sqlx_failure)?;
            actor_state(&stored)?
        }
        Some(Release::Terminal) => {
            sqlx::query(SQL.mail.delete_all.sql())
                .bind(actor)
                .execute(crate::observed_sql::executor(&mut ***tx))
                .await
                .map_err(sqlx_failure)?;
            let stored: String = sqlx::query_scalar(SQL.actor.end.sql())
                .bind(actor)
                .fetch_one(crate::observed_sql::executor(&mut ***tx))
                .await
                .map_err(sqlx_failure)?;
            actor_state(&stored)?
        }
        Some(rest) => {
            let (state, due) = match rest {
                Release::Waiting { next_due } => ("waiting", next_due.map(|due| due.0)),
                Release::Ready => ("ready", None),
                _ => ("idle", None),
            };
            let stored: String = sqlx::query_scalar(SQL.actor.release.sql())
                .bind(actor)
                .bind(state)
                .bind(due)
                .bind(now.0)
                .fetch_one(crate::observed_sql::executor(&mut ***tx))
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
/// registration) calls this in its own transaction, so
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
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let Some(row) = woke else {
        let state: Option<(i64, String)> = sqlx::query_as(SQL.actor.epoch_of.sql())
            .bind(actor.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
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

/// Wake session `session`'s actor inside a producer's transaction (ADR
/// 0132 §12), creating it ready first when the session has none yet.
pub(crate) async fn wake_session_within(
    tx: &mut PgConnection,
    session: &lash_sansio::SessionId,
    control: bool,
    now: DurableInstant,
) -> Result<Woken, DurableError> {
    let actor = ActorKey::session(session.as_str())
        .map_err(|error| corrupt("session actor key", &error.to_string()))?;
    sqlx::query(SQL.actor.create.sql())
        .bind(actor.as_str())
        .bind(actor.kind().as_str())
        .bind(lash_durable::domain::SESSION_ACTOR_FORMATS)
        .bind(now.0)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(wake_within(tx, &actor, control, now).await?.0)
}

/// [`wake_session_within`] for a session store transaction at `at_ms`. An
/// absent or deleted session, and one whose actor already ended (its close
/// finished), wakes nobody and creates nothing; that is no refusal of the
/// producer's write, which refuses such a session itself.
pub(crate) async fn wake_session_tx(
    tx: &mut PgConnection,
    session: &lash_sansio::SessionId,
    control: bool,
    at_ms: u64,
) -> Result<(), StoreError> {
    let wake = async {
        if !session_mail::standing(tx, session).await?.0 {
            return Ok(());
        }
        let now = DurableInstant(integer::<i64>(at_ms)?);
        match wake_session_within(tx, session, control, now).await {
            Ok(_) | Err(DurableError::MailRefused(MailRefusal::ActorTerminal(_))) => Ok(()),
            Err(error) => Err(error),
        }
    };
    wake.await.map_err(|error: DurableError| {
        StoreError::Backend(format!("session {session} was not woken: {error}"))
    })
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
        DomainWrite::SessionMail(write) => session_mail::apply(tx, committing, write).await,
        DomainWrite::Prompt(write) => prompts::apply(tx, committing, write).await,
    }
}

/// Apply one mailbox domain write by its domain's module, with its answer
/// and the actors it woke.
async fn apply_mail_domain(
    tx: &mut GuardedTx<'_>,
    write: &MailDomainWrite,
    now: DurableInstant,
    fleet: lash_core_execution::FleetFormat,
) -> Result<(MailAnswer, Vec<Woken>), DurableError> {
    Ok(match write {
        MailDomainWrite::ResolveWait(resolution) => {
            let (answer, woken) = waits::resolve(tx, resolution, now).await?;
            (MailAnswer::ResolveWait(answer), woken.into_iter().collect())
        }
        MailDomainWrite::RequestProcessCancel(request) => {
            let (answer, woken) = processes::request_cancel(tx, request, now, fleet).await?;
            (
                MailAnswer::RequestProcessCancel(answer),
                woken.into_iter().collect(),
            )
        }
        MailDomainWrite::RequestTurnCancel(request) => {
            let (answer, woken) = turns::request_cancel(tx, request, now).await?;
            (
                MailAnswer::RequestTurnCancel(answer),
                woken.into_iter().collect(),
            )
        }
        MailDomainWrite::Redrive(request) => {
            let (answer, woken) = park_events::redrive(tx, request, now).await?;
            (MailAnswer::Redrive(answer), woken.into_iter().collect())
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
            .execute(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    }
    Ok(())
}

async fn apply_mail(
    tx: &mut GuardedTx<'_>,
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
                    .fetch_optional(crate::observed_sql::executor(&mut ***tx))
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
                        .execute(crate::observed_sql::executor(&mut ***tx))
                        .await
                        .map_err(sqlx_failure)?;
                    receipt.appended.push((actor.clone(), seq));
                }
                note_woken(&mut receipt.woken, woken);
            }
            MailWrite::Domain(domain) => {
                let (answer, woken) = Box::pin(apply_mail_domain(tx, domain, now, fleet)).await?;
                receipt.answers.push(answer);
                for woken in woken {
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
        self.within(CommitCapacity::Work, async {
            let mut connection = self.reader().await?;
            self.instant_on(&mut connection).await
        })
        .await
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        self.within(CommitCapacity::Renewal, async {
            crate::observed_sql::measure(&self.observer, CommitLabel::NODE_REGISTER, 0, async {
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
                        .fetch_all(crate::observed_sql::executor(&mut **tx))
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
                        .execute(crate::observed_sql::executor(&mut **tx))
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
            })
            .await
        })
        .await
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        self.within(CommitCapacity::Renewal, async {
            crate::observed_sql::measure(&self.observer, CommitLabel::HEARTBEAT, 0, async {
                let (mut tx, now) = self.open(CommitLabel::HEARTBEAT).await?;
                let outcome = sqlx::query_scalar::<_, i64>(SQL.node.renew.sql())
                    .bind(node.owner.node.as_str())
                    .bind(node.owner.boot.as_str())
                    .bind(now.after_millis(node.ttl_millis).0)
                    .fetch_optional(crate::observed_sql::executor(&mut **tx))
                    .await
                    .map_err(sqlx_failure)
                    .map(|renewed| match renewed {
                        Some(expires_at) => HeartbeatOutcome::Renewed {
                            expires_at: DurableInstant(expires_at),
                        },
                        None => HeartbeatOutcome::Reaped,
                    });
                finish(tx, outcome).await
            })
            .await
        })
        .await
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        self.within(CommitCapacity::Critical, async {
            crate::observed_sql::measure(&self.observer, CommitLabel::REAP, 0, async {
                let (mut tx, now) = self.open(CommitLabel::REAP).await?;
                let outcome = async {
                    if !node_live(&mut tx, &reaper.owner).await? {
                        return Err(DurableError::NodeLeaseLost {
                            node: reaper.owner.node.clone(),
                        });
                    }
                    let dead: Vec<(String, String)> = sqlx::query_as(SQL.node.delete_expired.sql())
                        .bind(now.0)
                        .fetch_all(crate::observed_sql::executor(&mut **tx))
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
            })
            .await
        })
        .await
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        self.within(CommitCapacity::Critical, async {
            crate::observed_sql::measure(&self.observer, CommitLabel::NODE_RELEASE, 0, async {
                let (mut tx, now) = self.open(CommitLabel::NODE_RELEASE).await?;
                let outcome = async {
                    let deleted = sqlx::query(SQL.node.delete_boot.sql())
                        .bind(node.owner.node.as_str())
                        .bind(node.owner.boot.as_str())
                        .fetch_optional(crate::observed_sql::executor(&mut **tx))
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
            })
            .await
        })
        .await
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        self.within(CommitCapacity::Scheduler, async {
            crate::observed_sql::measure(&self.observer, CommitLabel::CLAIM, 0, async {
                let formats: Vec<String> = node
                    .decodes
                    .iter()
                    .map(|formats| formats.as_str().to_owned())
                    .collect();
                let limit = integer::<i64>(limit)?;
                let (mut tx, now) = self.open(CommitLabel::CLAIM).await?;
                let outcome = async {
                    match node_draining(&mut tx, &node.owner).await? {
                        None => {
                            return Err(DurableError::NodeLeaseLost {
                                node: node.owner.node.clone(),
                            });
                        }
                        Some(true) => return Ok(Vec::new()),
                        Some(false) => {}
                    }
                    let rows = sqlx::query(SQL.postgres.claim.sql())
                        .bind(now.0)
                        .bind(node.owner.node.as_str())
                        .bind(node.owner.boot.as_str())
                        .bind(&formats)
                        .bind(limit)
                        .bind(lash_durable::domain::CANCEL_MAIL)
                        .fetch_all(crate::observed_sql::executor(&mut **tx))
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
                                purpose: ClaimPurpose::of(&node.decodes, &get::<String>(row, 3)?),
                            })
                        })
                        .collect::<Result<Vec<_>, DurableError>>()?;
                    claimed.sort_by(|left, right| left.actor.cmp(&right.actor));
                    Ok(claimed)
                }
                .await;
                finish(tx, outcome).await
            })
            .await
        })
        .await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.within(CommitCapacity::Scheduler, async {
            let rows: Vec<(String, i64, String)> = sqlx::query_as(SQL.actor.owned_by.sql())
                .bind(node.owner.node.as_str())
                .bind(node.owner.boot.as_str())
                .fetch_all(crate::observed_sql::executor(&self.pools.scheduler))
                .await
                .map_err(sqlx_failure)?;
            rows.into_iter()
                .map(|(key, epoch, formats)| {
                    Ok(Claimed {
                        actor: actor_key(&key)?,
                        epoch: Epoch(epoch),
                        cause: ClaimCause::Adopted,
                        purpose: ClaimPurpose::of(&node.decodes, &formats),
                    })
                })
                .collect()
        })
        .await
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        self.within(CommitCapacity::Scheduler, async {
            crate::observed_sql::measure(&self.observer, CommitLabel::NODE_DRAIN, 0, async {
                let (mut tx, _now) = self.open(CommitLabel::NODE_DRAIN).await?;
                let outcome = async {
                    sqlx::query(SQL.node.mark_draining.sql())
                        .bind(node.owner.node.as_str())
                        .bind(node.owner.boot.as_str())
                        .fetch_optional(crate::observed_sql::executor(&mut **tx))
                        .await
                        .map_err(sqlx_failure)?
                        .map(|_| ())
                        .ok_or_else(|| DurableError::NodeLeaseLost {
                            node: node.owner.node.clone(),
                        })
                }
                .await;
                finish(tx, outcome).await
            })
            .await
        })
        .await
    }

    async fn live_decodes(&self) -> Result<Vec<Vec<FormatSet>>, DurableError> {
        self.within(CommitCapacity::Work, async {
            let mut connection = self.pools.work.acquire().await.map_err(sqlx_failure)?;
            let now = self.instant_on(&mut connection).await?;
            let rows: Vec<String> = sqlx::query_scalar(SQL.node.live_decodes.sql())
                .bind(now.0)
                .fetch_all(crate::observed_sql::executor(&mut *connection))
                .await
                .map_err(sqlx_failure)?;
            rows.iter().map(|stored| decoded_sets(stored)).collect()
        })
        .await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        self.within(CommitCapacity::Work, async {
            let session = match actor.kind() {
                lash_durable::ActorKind::Session => Some(actor.id()),
                lash_durable::ActorKind::Process => None,
            };
            let rows = sqlx::query(SQL.postgres.open.sql())
                .bind(actor.as_str())
                .bind(session)
                .fetch_all(crate::observed_sql::executor(&mut *self.reader().await?))
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
            let turn_cancel = match (session, get::<Option<String>>(first, 10)?) {
                (Some(session), Some(run)) => {
                    let session = lash_sansio::SessionId::try_from(session.to_owned())
                        .map_err(|_| corrupt("session id", session))?;
                    let run = lash_sansio::TurnId::try_from(run.clone())
                        .map_err(|_| corrupt("turn id", &run))?;
                    let stored = (
                        get(first, 11)?,
                        get(first, 12)?,
                        get(first, 13)?,
                        get(first, 14)?,
                        get(first, 15)?,
                    );
                    Some(turns::decode_cancel(&session, &run, stored)?)
                }
                _ => None,
            };
            let at = self.instant_or(get(first, 9)?)?;
            let mut opened = OpenedActor {
                actor: actor.clone(),
                epoch,
                revision: StateRevision(get(first, 2)?),
                acked: MailSeq(get(first, 3)?),
                seen: MailSeq(get(first, 4)?),
                mail: Vec::new(),
                at,
                turn_cancel,
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
        })
        .await
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        self.commit_owner(tx, label).await
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        self.commit_mailbox(tx, label).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.within(CommitCapacity::Work, async {
            let Some(row) = sqlx::query(SQL.actor.snapshot.sql())
                .bind(actor.as_str())
                .fetch_optional(crate::observed_sql::executor(&self.pools.work))
                .await
                .map_err(sqlx_failure)?
            else {
                return Ok(None);
            };

            let mail_seq: i64 = get(&row, 5)?;
            let acked_seq: i64 = get(&row, 6)?;
            Ok(Some(ActorSnapshot {
                actor: actor.clone(),
                state: actor_state(&get::<String>(&row, 1)?)?,
                epoch: Epoch(get(&row, 2)?),
                owner: owner(get(&row, 3)?, get(&row, 4)?),
                has_mail: mail_seq > acked_seq,
                next_due: get::<Option<i64>>(&row, 7)?.map(DurableInstant),
                revision: StateRevision(get(&row, 8)?),
                formats: FormatSet::new(get::<String>(&row, 9)?),
                pending_mail: integer::<u64>(get::<i64>(&row, 10)?)?,
                park: get(&row, 11)?,
                failed_activations: integer::<u32>(get::<i64>(&row, 12)?)?,
            }))
        })
        .await
    }
}

impl PostgresDurableStore {
    /// A pooled connection for one unfenced domain read.
    async fn reader(&self) -> Result<sqlx::pool::PoolConnection<sqlx::Postgres>, DurableError> {
        crate::observed_sql::checkout(&self.pools.work)
            .await
            .map_err(sqlx_failure)
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

/// The hot-set law's one-shot serialization failure, on the claim's epoch write.
#[cfg(test)]
const CONTEND_FIRST_CLAIM_SQL: &str = "CREATE SEQUENCE claim_attempts;
CREATE FUNCTION contend_first_claim() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF nextval('claim_attempts') = 1 THEN
        RAISE EXCEPTION 'contended claim' USING ERRCODE = '40001';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER contend_first_claim BEFORE UPDATE OF epoch ON lash_actors
    FOR EACH ROW EXECUTE FUNCTION contend_first_claim();";

#[cfg(test)]
#[path = "../durable_concurrency_tests.rs"]
mod concurrency_tests;

#[cfg(test)]
#[path = "../host_guard_tests.rs"]
mod host_guard_tests;

#[cfg(test)]
#[path = "../durable_retry_tests.rs"]
mod retry_tests;

#[cfg(test)]
mod constraint_tests {
    use sqlx::Connection as _;

    #[tokio::test]
    async fn durable_waits_and_actors_refuse_impossible_rows() {
        let url = crate::testing::required_database_url();
        let database = crate::testing::IsolatedDatabase::create(&url).await;
        let mut conn = sqlx::PgConnection::connect(database.url())
            .await
            .expect("open constraint fixture");
        sqlx::raw_sql(crate::PostgresStorage::schema_ddl())
            .execute(crate::observed_sql::executor(&mut conn))
            .await
            .expect("create constraint fixture");
        sqlx::query("INSERT INTO lash_waits (wait_id, owner_actor, owner_scope, kind, host_resolvable, state, created_epoch, key_name) VALUES ('wait', 's/session', 's/session', 'engine_key', true, 'pending', 1, 'key')").execute(crate::observed_sql::executor(&mut conn)).await.expect("pending engine key wait");
        for (assignment, constraint) in [
            (
                "kind = 'timer', host_resolvable = false, key_name = NULL",
                "ck_waits_timer_deadline",
            ),
            ("key_name = NULL", "ck_waits_key_name"),
            ("call_id = 'call', tool_id = 'tool'", "ck_waits_call"),
            ("resolved_at_ms = 1", "ck_waits_settled_at"),
            ("state = 'revoked'", "ck_waits_settled_at"),
            (
                "state = 'resolved', resolution_digest = 'digest', resolved_at_ms = 1",
                "ck_waits_resolution_ref",
            ),
            ("resolution_ref = 'payload'", "ck_waits_resolution_ref"),
        ] {
            let error = sqlx::query(&format!("UPDATE lash_waits SET {assignment}"))
                .execute(crate::observed_sql::executor(&mut conn))
                .await
                .expect_err("impossible wait must be refused");
            assert_eq!(
                error
                    .as_database_error()
                    .and_then(sqlx::error::DatabaseError::constraint),
                Some(constraint)
            );
        }
        sqlx::query("UPDATE lash_waits SET kind = 'timer', host_resolvable = false, key_name = NULL, deadline_ms = 1, state = 'resolved', resolution_digest = 'timer', resolved_at_ms = 1").execute(crate::observed_sql::executor(&mut conn)).await.expect("resolved timer without value");
        let error = sqlx::query("UPDATE lash_waits SET resolution_ref = 'payload'")
            .execute(crate::observed_sql::executor(&mut conn))
            .await
            .expect_err("timer has no payload");
        assert_eq!(
            error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::constraint),
            Some("ck_waits_resolution_ref")
        );
        for (key, kind) in [
            ("s/session", "process"),
            ("p/process", "session"),
            ("x/unknown", "session"),
        ] {
            let error = sqlx::query(&format!("INSERT INTO lash_actors (actor_key, kind, state, epoch, formats, state_revision, mail_seq, acked_seq, created_at_ms) VALUES ('{key}', '{kind}', 'idle', 0, '[]', 0, 0, 0, 0)")).execute(crate::observed_sql::executor(&mut conn)).await.expect_err("key must agree with kind");
            assert_eq!(
                error
                    .as_database_error()
                    .and_then(sqlx::error::DatabaseError::constraint),
                Some("ck_lash_actors_key_kind")
            );
        }
        for version in [-1_i64, i64::from(u32::MAX) + 1] {
            let error = sqlx::query("INSERT INTO lash_exec_snapshots VALUES ('p/process', 1, 'snapshot', 'identity', $1, 1)").bind(version).execute(crate::observed_sql::executor(&mut conn)).await.expect_err("format versions are u32");
            assert_eq!(
                error
                    .as_database_error()
                    .and_then(sqlx::error::DatabaseError::constraint),
                Some("ck_exec_snapshots_format_version")
            );
        }
    }
}

#[cfg(test)]
#[path = "../durable_round_trip_tests.rs"]
mod round_trip_tests;
