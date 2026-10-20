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

#[cfg(feature = "testing")]
mod test_writer;

use std::sync::{Arc, LazyLock};

use lash_durable::{
    ActorCommit, ActorKey, ActorSnapshot, ActorState, ActorTx, BootId, ClaimCause, ClaimPurpose,
    Claimed, CommitCapacity, CommitLabel, DomainWrite, DurableError, DurableInstant, DurableStore,
    Epoch, Fenced, FormatSet, HeartbeatOutcome, Mail, MailAnswer, MailCommit, MailDomainWrite,
    MailKind, MailRefusal, MailSeq, MailTx, MailWrite, NodeId, NodeLease, NodeSpec, OpenedActor,
    Owner, Reaped, StateRevision, StoreFailure, StoreFailureKind, Woken,
};
use lash_store_sql::Dialect;
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
mod executor;
pub(crate) use executor::HeldTexts;
#[cfg(test)]
mod generator;
#[cfg(test)]
pub(crate) use generator::peel_loop;
mod claim_facts;
mod graph;
mod head_commit;
mod packages;
pub(crate) use graph::{
    AUDIT_PAGE, GraphClosure, audit_page, graph_closure, graph_counts, install_staged, peel,
};
pub(crate) use head_commit::{SessionHints, apply_store_commit};
mod pages;
#[cfg(feature = "testing")]
pub(crate) use pages::cut_retirement_holds;
pub(crate) use pages::retire_roots;
mod park;
mod process_view;
pub(crate) mod processes;
mod prompts;
pub(crate) mod retention;
pub(crate) use prompts::release as release_prompt_snapshots;
mod model_calls;
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
mod steps;
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
    pub(crate) fleet: lash_core_store::store::FleetFormat,
    /// The roots its writes retired, held before it commits.
    pub(crate) retired: Retired,
}

impl Committing<'_> {
    /// What the commit's step programs run under.
    fn steps(&self) -> steps::Context<'_> {
        steps::Context {
            retired: &self.retired,
            actor: self.actor,
            now: self.now,
        }
    }
}

/// The roots a transaction's writes retired: what it holds for the
/// retirement steps before it commits (FIG-5949, FIG-6191). A turn's
/// terminal retires its PhaseRoot alone: the TurnContent under it is the
/// transcript the session's next turn names again, so it waits for the
/// orphan audit rather than being reinstalled (the TurnContent ordering
/// constraint).
#[derive(Default)]
pub(crate) struct Retired {
    roots: std::sync::Mutex<(
        std::collections::BTreeSet<String>,
        std::collections::BTreeSet<String>,
    )>,
}

impl Retired {
    fn record(&self, phase_end: bool, root: String) {
        let mut roots = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if phase_end {
            roots.1.insert(root);
        } else {
            roots.0.insert(root);
        }
    }

    /// A stored root reference was retired: retire its closure.
    pub(crate) fn stored(&self, root: impl Into<String>) {
        self.record(false, root.into());
    }

    /// An ended turn's PhaseRoot was retired: retire it alone.
    pub(crate) fn phase_end(&self, root: impl Into<String>) {
        self.record(true, root.into());
    }

    /// The retired closures, then the ended PhaseRoots.
    pub(crate) fn into_parts(
        self,
    ) -> (
        std::collections::BTreeSet<String>,
        std::collections::BTreeSet<String>,
    ) {
        self.roots
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

mod scheduling_sql;
use scheduling_sql::PostgresDurableStatements;

struct Sql {
    node: NodeStatements,
    actor: ActorStatements,
    mail: MailStatements,
    park: ActorParkStatements,
    process: ProcessActorStatements,
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
        session_mail: SessionMailStatements::render(
            dialect.with_vocabulary(crate::turn_ingress::TURN_INPUT_LIFECYCLE),
        ),
        postgres: PostgresDurableStatements::render(dialect),
    }
});

/// The [`DurableStore`] over one PostgreSQL catalog.
#[derive(Clone)]
pub struct PostgresDurableStore {
    pools: Arc<RolePools>,
    fence: WriterFence,
    observer: lash_core_ids::operational_metrics::StoreObserver,
    /// The clock a test stands in for the database's.
    #[cfg(any(test, feature = "testing"))]
    clock: Option<std::sync::Arc<dyn lash_core_ids::clock::Clock>>,
    /// A connection a test loses at the next commit's `COMMIT`.
    #[cfg(any(test, feature = "testing"))]
    commit_fault: Option<Arc<crate::testing::CommitFault>>,
    /// Where a test holds a transaction inside its SQL.
    #[cfg(any(test, feature = "testing"))]
    storage_gates: Option<crate::testing::StorageGates>,
}

impl std::fmt::Debug for PostgresDurableStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresDurableStore")
            .finish_non_exhaustive()
    }
}

type Tx = GuardedTx<'static>;

/// Delete every engine row `session` owns, in its physical delete: its
/// turns' run facts (their tool-call ledgers), its cells' roots, admissions
/// and facts, the waits its actor owns and its turns' wait tombstones. The
/// session's actor row stays: its terminal fence, which session write
/// locking takes first. Answers every graph root those rows held: the
/// delete's to peel.
pub(crate) async fn retire_session_rows(
    tx: &mut PgConnection,
    session: &lash_protocol_types::SessionId,
) -> Result<Vec<String>, sqlx::Error> {
    use lash_durable::domain::{ExecKey, OwnerKey, ScopeKey};

    let cells = ExecKey::session_cells(session);
    let mut roots = run_records::delete_under(&mut *tx, &OwnerKey::session_turns(session)).await?;
    roots.extend(model_calls::delete_session(&mut *tx, session).await?);
    roots.extend(snapshots::retire_under(&mut *tx, &cells).await?);
    if let Ok(actor) = ActorKey::session(session.as_str()) {
        waits::delete_owned(&mut *tx, &actor).await?;
    }
    waits::delete_under(tx, &ScopeKey::session_turns(session)).await?;
    Ok(roots)
}

/// Delete `process`'s run facts and the waits it owns, in its prune: the
/// process host API's tool calls go with its record. Answers the RoundFact
/// nodes they held: the prune's to peel.
pub(crate) async fn retire_process_rows(
    tx: &mut PgConnection,
    process: &lash_protocol_types::ProcessId,
) -> Result<Vec<String>, sqlx::Error> {
    let mut facts = run_records::delete_owner(
        &mut *tx,
        &lash_durable::domain::OwnerKey::Process(process.clone()).stored(),
    )
    .await?;
    facts.extend(
        model_calls::delete_session(
            tx,
            &lash_protocol_types::SessionId::prefixed("process:", process.as_str()),
        )
        .await?,
    );
    if let Ok(actor) = ActorKey::process(process.as_str()) {
        waits::delete_owned(tx, &actor).await?;
    }
    Ok(facts)
}

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
        // Bytes a write's own read cannot decode never clear by themselves.
        StoreError::StoredDataCorrupt { .. } => StoreFailureKind::Corrupt,
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

/// A stored checkpoint graph reference: its node's lowercase hex.
fn node_ref(stored: &str) -> Result<lash_core_store::checkpoint_graph::NodeId, DurableError> {
    lash_core_store::checkpoint_graph::NodeId::from_hex(stored)
        .ok_or_else(|| corrupt("node reference", stored))
}

fn actor_key(stored: &str) -> Result<ActorKey, DurableError> {
    ActorKey::parse(stored).map_err(|_| corrupt("actor key", stored))
}

/// An actor's stored claim requirements.
fn requires_of(stored: Option<String>) -> Result<Vec<String>, DurableError> {
    lash_durable::parse_requires(stored.as_deref())
        .map_err(|error| corrupt("actor claim requirements", &error.to_string()))
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
        observer: lash_core_ids::operational_metrics::StoreObserver,
    ) -> Self {
        Self {
            pools,
            fence,
            observer,
            #[cfg(any(test, feature = "testing"))]
            clock: None,
            #[cfg(any(test, feature = "testing"))]
            commit_fault: None,
            #[cfg(any(test, feature = "testing"))]
            storage_gates: None,
        }
    }

    /// Read `clock` where the database clock would be read: a test moves
    /// time instead of waiting for it.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_clock_for_testing(
        mut self,
        clock: std::sync::Arc<dyn lash_core_ids::clock::Clock>,
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

    /// Hold this store's transactions where `gates` arms a pause.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_storage_gates_for_testing(mut self, gates: crate::testing::StorageGates) -> Self {
        self.storage_gates = Some(gates);
        self
    }

    /// Wait at `point` while a test's storage gate holds it.
    #[cfg(any(test, feature = "testing"))]
    async fn storage_point(&self, point: crate::testing::StoragePoint) {
        if let Some(gates) = &self.storage_gates {
            gates.pass(point).await;
        }
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
        let now: i64 = crate::observed_sql::query_scalar(
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
            crate::observed_sql::query_as(SQL.postgres.fence_and_clock.sql())
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
            let rows: Vec<(String, String, bool)> =
                crate::observed_sql::query_as(SQL.postgres.liveness.sql())
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
                        let taken: bool =
                            crate::observed_sql::query_scalar(SQL.postgres.take_liveness.sql())
                                .bind(owner.boot.as_str())
                                .fetch_one(crate::observed_sql::executor(&mut **tx))
                                .await
                                .map_err(sqlx_failure)?;
                        if taken != free {
                            return Ok(Vec::new());
                        }
                    }
                    let deleted = crate::observed_sql::query(SQL.node.delete_boot.sql())
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
    crate::observed_sql::query_scalar(SQL.postgres.node_live.sql())
        .bind(node.node.as_str())
        .bind(node.boot.as_str())
        .fetch_optional(crate::observed_sql::executor(tx))
        .await
        .map_err(sqlx_failure)
}

async fn release_owned_by(
    tx: &mut PgConnection,
    owner: &Owner,
    now: DurableInstant,
) -> Result<Vec<(ActorKey, Epoch)>, DurableError> {
    let rows = crate::observed_sql::query(SQL.actor.release_owned_by.sql())
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
    let current: Option<i64> = crate::observed_sql::query_scalar(SQL.actor.epoch_of.sql())
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
                DomainWrite::RunFact(lash_durable::domain::RunFactWrite::Append {
                    kind: lash_durable::domain::RunFactKind::XOutcome,
                    ..
                })
            )
        })
        .count() as u64
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
    let woke = crate::observed_sql::query(statement)
        .bind(actor.as_str())
        .bind(now.0)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let Some(row) = woke else {
        let state: Option<(i64, String)> = crate::observed_sql::query_as(SQL.actor.epoch_of.sql())
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
    session: &lash_protocol_types::SessionId,
    control: bool,
    now: DurableInstant,
) -> Result<Woken, DurableError> {
    let actor = ActorKey::session(session.as_str())
        .map_err(|error| corrupt("session actor key", &error.to_string()))?;
    crate::observed_sql::query(SQL.actor.create.sql())
        .bind(actor.as_str())
        .bind(actor.kind().as_str())
        .bind(lash_durable::domain::SESSION_ACTOR_FORMATS)
        .bind(now.0)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    Ok(wake_within(tx, &actor, control, now).await?.0)
}

/// [`wake_session_within`] for a session store transaction at `at_ms`,
/// answering the actor it woke for the caller to hand to the storage's wake
/// sink once the transaction commits. An absent or deleted session, and one
/// whose actor already ended (its close finished), wakes nobody and creates
/// nothing; that is no refusal of the producer's write, which refuses such a
/// session itself.
pub(crate) async fn wake_session_tx(
    tx: &mut PgConnection,
    session: &lash_protocol_types::SessionId,
    control: bool,
    at_ms: u64,
) -> Result<Option<Woken>, StoreError> {
    let wake = async {
        if !session_mail::standing(tx, session).await?.0 {
            return Ok(None);
        }
        let now = DurableInstant(integer::<i64>(at_ms)?);
        match wake_session_within(tx, session, control, now).await {
            Ok(woken) => Ok(Some(woken)),
            Err(DurableError::MailRefused(MailRefusal::ActorTerminal(_))) => Ok(None),
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
        DomainWrite::RunFact(write) => run_records::apply(tx, committing, write).await,
        DomainWrite::ExecRoot(write) => snapshots::apply(tx, committing, write).await,
        DomainWrite::EffectAdmission(write) => graph::apply_admission(tx, committing, write).await,
        DomainWrite::Wait(write) => waits::apply(tx, committing, write).await,
        DomainWrite::Process(write) => processes::apply(tx, committing, write).await,
        DomainWrite::SessionClose(write) => session_close::apply(tx, committing, write).await,
        DomainWrite::ActorPark { reason_json } => {
            park::set(tx, committing.actor.as_str(), reason_json).await
        }
        DomainWrite::SessionMail(write) => session_mail::apply(tx, committing, write).await,
        DomainWrite::Prompt(write) => prompts::apply(tx, committing, write).await,
        DomainWrite::ModelCallPin(write) => model_calls::apply_pin(tx, write).await,
        DomainWrite::ModelCall(write) => model_calls::apply(tx, committing, write).await,
    }
}

/// Apply one mailbox domain write by its domain's module, with its answer
/// and the actors it woke.
async fn apply_mail_domain(
    tx: &mut GuardedTx<'_>,
    write: &MailDomainWrite,
    now: DurableInstant,
    fleet: lash_core_store::store::FleetFormat,
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
        MailDomainWrite::RequestOperationCancel(request) => {
            let (answer, woken) = session_mail::request_operation_cancel(tx, request, now).await?;
            (
                MailAnswer::RequestOperationCancel(answer),
                woken.into_iter().collect(),
            )
        }
        MailDomainWrite::Redrive(request) => {
            let (answer, woken) = park::redrive(tx, request, now).await?;
            (MailAnswer::Redrive(answer), woken.into_iter().collect())
        }
        MailDomainWrite::RedriveSessionParks(redrive) => {
            let (redriven, woken) = park::redrive_session_parks(tx, redrive, now).await?;
            (MailAnswer::RedriveSessionParks(redriven), woken)
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
/// its own order after these; a process cancel locks the process's actor
/// before its registry row, as the actor's owner commit does.
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
        crate::observed_sql::query(SQL.postgres.lock_actors.sql())
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
    fleet: lash_core_store::store::FleetFormat,
) -> Result<MailCommit, DurableError> {
    lock_mail_targets(tx, writes).await?;
    let mut receipt = MailCommit::default();
    for write in writes.writes() {
        match write {
            MailWrite::CreateActor { actor, formats } => {
                let created = crate::observed_sql::query(SQL.actor.create.sql())
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
                let (woken, seq) = wake_within(tx, actor, write.controls(), now).await?;
                if let MailWrite::Append { kind, body, .. } = write {
                    crate::observed_sql::query(SQL.mail.append.sql())
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
        self.read(|connection| Box::pin(self.instant_on(connection)))
            .await
    }

    fn pages(&self) -> Option<std::sync::Arc<dyn lash_durable::domain::pages::PageStore>> {
        Some(std::sync::Arc::new(self.clone()))
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
                let capabilities = lash_durable::packages::encode_capabilities(&spec.capabilities);
                let (mut tx, now) = self.open(CommitLabel::NODE_REGISTER).await?;
                let outcome = async {
                    let boots: Vec<String> =
                        crate::observed_sql::query_scalar(SQL.node.delete_boots.sql())
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
                    crate::observed_sql::query(SQL.node.insert.sql())
                        .bind(lease_owner.node.as_str())
                        .bind(lease_owner.boot.as_str())
                        .bind(&formats)
                        .bind(now.0)
                        .bind(expires_at.0)
                        .bind(&capabilities)
                        .execute(crate::observed_sql::executor(&mut **tx))
                        .await
                        .map_err(sqlx_failure)?;
                    packages::hold(&mut tx, lease_owner.node.as_str(), &spec.held).await?;
                    Ok(NodeLease {
                        owner: lease_owner,
                        decodes: spec.decodes.clone(),
                        serves: spec.serves.clone(),
                        held: spec.held.clone(),
                        capabilities: spec.capabilities.clone(),
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
                let outcome = crate::observed_sql::query_scalar::<_, i64>(SQL.node.renew.sql())
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
                    let dead: Vec<(String, String)> =
                        crate::observed_sql::query_as(SQL.node.delete_expired.sql())
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
                    let deleted = crate::observed_sql::query(SQL.node.delete_boot.sql())
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

    #[cfg(feature = "testing")]
    async fn acquire_idle_for_testing(
        &self,
        node: &NodeLease,
        actor: &ActorKey,
        epoch: Epoch,
    ) -> Result<Epoch, DurableError> {
        self.acquire_test_writer(node, actor, epoch).await
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
                    #[cfg(any(test, feature = "testing"))]
                    self.storage_point(crate::testing::StoragePoint::ClaimAfterNodeLive)
                        .await;
                    let rows = crate::observed_sql::query(SQL.postgres.claim.sql())
                        .bind(now.0)
                        .bind(node.owner.node.as_str())
                        .bind(node.owner.boot.as_str())
                        .bind(&formats)
                        .bind(limit)
                        .bind(lash_durable::domain::CANCEL_MAIL)
                        .bind(&node.serves)
                        .bind(lash_durable::domain::SESSION_CLOSE_MAIL)
                        .fetch_all(crate::observed_sql::executor(&mut **tx))
                        .await
                        .map_err(sqlx_failure)?;
                    let mut claimed = rows
                        .iter()
                        .map(|row| {
                            let requires = requires_of(get::<Option<String>>(row, 4)?)?;
                            Ok(Claimed {
                                actor: actor_key(&get::<String>(row, 0)?)?,
                                epoch: Epoch(get(row, 1)?),
                                cause: if get::<String>(row, 2)? == ActorState::Waiting.as_str() {
                                    ClaimCause::Due
                                } else {
                                    ClaimCause::Ready
                                },
                                purpose: ClaimPurpose::of(
                                    node,
                                    &get::<String>(row, 3)?,
                                    &requires,
                                    get::<bool>(row, 5)?,
                                ),
                                requires,
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
            let rows: Vec<(String, i64, String, Option<String>, bool)> =
                crate::observed_sql::query_as(SQL.actor.owned_by.sql())
                    .bind(node.owner.node.as_str())
                    .bind(node.owner.boot.as_str())
                    .fetch_all(crate::observed_sql::executor(&self.pools.scheduler))
                    .await
                    .map_err(sqlx_failure)?;
            rows.into_iter()
                .map(|(key, epoch, formats, requires, holds)| {
                    let requires = requires_of(requires)?;
                    Ok(Claimed {
                        actor: actor_key(&key)?,
                        epoch: Epoch(epoch),
                        cause: ClaimCause::Adopted,
                        purpose: ClaimPurpose::of(node, &formats, &requires, holds),
                        requires,
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
                    crate::observed_sql::query(SQL.node.mark_draining.sql())
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

    async fn live_nodes(&self) -> Result<Vec<lash_durable::NodeCapability>, DurableError> {
        self.within(CommitCapacity::Work, async {
            let mut connection = self.pools.work.acquire().await.map_err(sqlx_failure)?;
            let now = self.instant_on(&mut connection).await?;
            packages::live_nodes(&mut connection, now).await
        })
        .await
    }

    async fn install_packages(
        &self,
        artifacts: &[lash_durable::packages::ArtifactInstall],
        sets: &[lash_durable::packages::SetInstall],
    ) -> Result<(), DurableError> {
        self.within(CommitCapacity::Work, async {
            crate::observed_sql::measure(&self.observer, CommitLabel::PACKAGES_INSTALL, 0, async {
                let (mut tx, _now) = self.open(CommitLabel::PACKAGES_INSTALL).await?;
                let outcome = packages::install(&mut tx, artifacts, sets).await;
                finish(tx, outcome).await
            })
            .await
        })
        .await
    }

    async fn package_catalog(&self) -> Result<lash_durable::PackageCatalog, DurableError> {
        self.within(CommitCapacity::Work, async {
            let mut connection = self.pools.work.acquire().await.map_err(sqlx_failure)?;
            packages::catalog(&mut connection).await
        })
        .await
    }

    async fn package_surface(
        &self,
        digest: &lash_durable::PackageSetDigest,
    ) -> Result<Option<Vec<u8>>, DurableError> {
        self.within(CommitCapacity::Work, async {
            let mut connection = self.pools.work.acquire().await.map_err(sqlx_failure)?;
            packages::surface(&mut connection, &digest.to_string()).await
        })
        .await
    }

    async fn package_paths(
        &self,
        functions: &[lash_durable::packages::FunctionId],
    ) -> Result<
        std::collections::BTreeMap<
            lash_durable::packages::FunctionId,
            std::collections::BTreeSet<String>,
        >,
        DurableError,
    > {
        self.within(CommitCapacity::Work, async {
            let mut connection = self.pools.work.acquire().await.map_err(sqlx_failure)?;
            packages::paths(&mut connection, packages::Index::Definitions, functions).await
        })
        .await
    }

    async fn package_labels(
        &self,
        artifacts: &[lash_durable::ArtifactDigest],
    ) -> Result<std::collections::BTreeMap<lash_durable::ArtifactDigest, Vec<String>>, DurableError>
    {
        self.within(CommitCapacity::Work, async {
            let mut connection = self.pools.work.acquire().await.map_err(sqlx_failure)?;
            packages::labels(&mut connection, artifacts).await
        })
        .await
    }

    async fn internal_operations(
        &self,
        operations: &[lash_durable::packages::EffectName],
    ) -> Result<
        std::collections::BTreeMap<
            lash_durable::packages::EffectName,
            std::collections::BTreeSet<String>,
        >,
        DurableError,
    > {
        self.within(CommitCapacity::Work, async {
            let mut connection = self.pools.work.acquire().await.map_err(sqlx_failure)?;
            packages::paths(
                &mut connection,
                packages::Index::InternalOperations,
                operations,
            )
            .await
        })
        .await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        let session = match actor.kind() {
            lash_durable::ActorKind::Session => Some(actor.id()),
            lash_durable::ActorKind::Process => None,
        };
        let rows = self
            .read(|connection| {
                Box::pin(async move {
                    crate::observed_sql::query(SQL.postgres.open.sql())
                        .bind(actor.as_str())
                        .bind(session)
                        .fetch_all(crate::observed_sql::executor(&mut **connection))
                        .await
                        .map_err(sqlx_failure)
                })
            })
            .await?;
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
                let session = lash_protocol_types::SessionId::try_from(session.to_owned())
                    .map_err(|_| corrupt("session id", session))?;
                let run = lash_protocol_types::TurnId::try_from(run.clone())
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
        // What a session's owner read of its head: the hint its one-call
        // head commit plans on (FIG-5942).
        if session.is_some()
            && let Some(facts) = head_commit::SessionFacts::of(first, 16)
        {
            self.pools
                .session_hints
                .note((actor.as_str().to_owned(), epoch.0), facts);
        }
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
    }

    async fn open_process(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
    ) -> Result<(ActorTx, lash_durable::domain::ProcessView), DurableError> {
        self.open_process_view(actor, epoch).await
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

    async fn claim_facts(
        &self,
        node: Option<&Owner>,
        actor: &ActorKey,
    ) -> Result<lash_durable::ClaimFacts, DurableError> {
        self.read_claim_facts(node, actor).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.within(CommitCapacity::Work, async {
            let Some(row) = crate::observed_sql::query(SQL.actor.snapshot.sql())
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

    /// Every owner commit's fence updates its actor's row first and holds
    /// it to the commit's end, so a share lock on the row waits for the
    /// commit in flight. Under READ COMMITTED a commit sees only rows
    /// committed when its statement ran: the terminal transaction of a
    /// process misses a wait pinned on it meanwhile, and the awaiter's
    /// read after its pin misses the terminal until it commits (FIG-6329).
    async fn await_owner_commit(&self, actor: &ActorKey) -> Result<(), DurableError> {
        self.within(CommitCapacity::Work, async {
            crate::observed_sql::query(SQL.postgres.await_owner_commit.sql())
                .bind(actor.as_str())
                .fetch_optional(crate::observed_sql::executor(&self.pools.work))
                .await
                .map(drop)
                .map_err(sqlx_failure)
        })
        .await
    }

    async fn actors_in(
        &self,
        formats: &FormatSet,
        after: Option<&ActorKey>,
        limit: usize,
    ) -> Result<Vec<ActorKey>, DurableError> {
        self.within(CommitCapacity::Work, async {
            let rows = crate::observed_sql::query(SQL.actor.in_formats.sql())
                .bind(formats.as_str())
                .bind(after.map_or("", ActorKey::as_str))
                .bind(i64::try_from(limit).unwrap_or(i64::MAX))
                .fetch_all(crate::observed_sql::executor(&self.pools.work))
                .await
                .map_err(sqlx_failure)?;
            rows.iter()
                .map(|row| actor_key(&get::<String>(row, 0)?))
                .collect()
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

/// A `REPEATABLE READ READ ONLY` transaction on `connection`, for an
/// unfenced read of several statements: one snapshot, so a root a peel
/// retires during the read is read whole.
async fn snapshot(
    connection: &mut PgConnection,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, DurableError> {
    let mut snapshot = crate::observed_sql::control("BEGIN", sqlx::Connection::begin(connection))
        .await
        .map_err(sqlx_failure)?;
    crate::observed_sql::query(
        crate::connection_sql::connection_sql()
            .begin_repeatable_read_read_only
            .sql(),
    )
    .execute(crate::observed_sql::executor(&mut *snapshot))
    .await
    .map_err(sqlx_failure)?;
    Ok(snapshot)
}

#[cfg(test)]
#[path = "../durable_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "../wait_law_tests.rs"]
mod wait_law_tests;

#[cfg(test)]
mod terminal_wait_tests;

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

/// The contended-cancel law's one-shot deadlock, on the cancel's mail
/// append (FIG-5855).
#[cfg(test)]
const DEADLOCK_FIRST_CANCEL_MAIL_SQL: &str = "CREATE SEQUENCE cancel_attempts;
CREATE FUNCTION deadlock_first_cancel_mail() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.kind = 'cancel' THEN
        IF nextval('cancel_attempts') = 1 THEN
            RAISE EXCEPTION 'injected cancel deadlock' USING ERRCODE = '40P01';
        END IF;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER deadlock_first_cancel_mail BEFORE INSERT ON lash_actor_mail
    FOR EACH ROW EXECUTE FUNCTION deadlock_first_cancel_mail();";

/// The lock-order law's owner commit, at its fence: actor `$1`'s row
/// (FIG-5855).
#[cfg(test)]
const OWNER_FENCE_SQL: &str =
    "UPDATE lash_actors SET state_revision = state_revision + 1 WHERE actor_key = $1";

/// The claim-availability law's holder: actor `$1`'s row, locked as a
/// concurrent transaction holds it (FIG-6131).
#[cfg(test)]
const HOLD_ACTOR_SQL: &str = "SELECT actor_key FROM lash_actors WHERE actor_key = $1 FOR UPDATE";

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
        crate::observed_sql::query("INSERT INTO lash_waits (wait_id, owner_actor, owner_scope, kind, host_resolvable, state, created_epoch, key_name) VALUES ('wait', 's/session', 's/session', 'engine_key', true, 'pending', 1, 'key')").execute(crate::observed_sql::executor(&mut conn)).await.expect("pending engine key wait");
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
            let error = crate::observed_sql::query(&format!("UPDATE lash_waits SET {assignment}"))
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
        crate::observed_sql::query("UPDATE lash_waits SET kind = 'timer', host_resolvable = false, key_name = NULL, deadline_ms = 1, state = 'resolved', resolution_digest = 'timer', resolved_at_ms = 1").execute(crate::observed_sql::executor(&mut conn)).await.expect("resolved timer without value");
        let error = crate::observed_sql::query("UPDATE lash_waits SET resolution_ref = 'payload'")
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
            let error = crate::observed_sql::query(&format!("INSERT INTO lash_actors (actor_key, kind, state, epoch, formats, state_revision, mail_seq, acked_seq, created_at_ms) VALUES ('{key}', '{kind}', 'idle', 0, '[]', 0, 0, 0, 0)")).execute(crate::observed_sql::executor(&mut conn)).await.expect_err("key must agree with kind");
            assert_eq!(
                error
                    .as_database_error()
                    .and_then(sqlx::error::DatabaseError::constraint),
                Some("ck_lash_actors_key_kind")
            );
        }
        for (rev, commit_id, constraint) in [
            (0_i64, vec![0_u8; 32], "ck_exec_snapshots_rev"),
            (1, vec![0; 31], "ck_exec_snapshots_commit_id"),
        ] {
            let error = crate::observed_sql::query("INSERT INTO lash_exec_snapshots (exec_key, rev, root_ref, commit_id, written_epoch) VALUES ('p/process', $1, 'root', $2, 1)").bind(rev).bind(commit_id).execute(crate::observed_sql::executor(&mut conn)).await.expect_err("rev and commit id are checked");
            assert_eq!(
                error
                    .as_database_error()
                    .and_then(sqlx::error::DatabaseError::constraint),
                Some(constraint)
            );
        }
    }
}

#[cfg(test)]
#[path = "../durable_round_trip_tests.rs"]
mod round_trip_tests;
