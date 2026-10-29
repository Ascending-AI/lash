//! The durability engine over SQLite: [`SqliteDurableStore`], its DDL and the
//! statements that fork from the neutral set in `lash_store_sql::durable`.
//!
//! This is the SQLite engine module: no other file in this crate may name the
//! engine's tables (`scripts/check-durable-sql.py`). Each domain the runtime
//! lanes add to the fenced commit has one submodule here, owned by its lane;
//! this module dispatches each [`DomainWrite`] and [`MailDomainWrite`] to it
//! and delegates each [`DurableReads`] read to it.
//!
//! Every write runs in [`SqliteConnection::write_flow`], so it holds the
//! database's writer gate and `BEGIN IMMEDIATE` and passes the writer fence
//! first. `BEGIN IMMEDIATE` takes the database's write lock for every
//! connection of every process on the file, so writers serialize and a claim
//! needs no row lock: two nodes, in one process or in two, never take one
//! actor twice. Instants come from the store set's injected clock, read once
//! per transaction; in production that is the machine's clock, which every
//! process on the file shares and SQLite's own time functions read.
//!
//! Node wakes ([`SqliteNodeWakes`]) live beside this module: wake rows
//! captured in the producer transaction and signalled after commit, and each
//! boot's liveness lock, a file lock its listener holds
//! ([`crate::liveness_locks`]).

#[cfg(feature = "testing")]
mod test_writer;

mod owner;
use owner::apply_owner;

use std::sync::{Arc, LazyLock};

use lash_core_ids::clock::Clock;
use lash_core_store::checkpoint_graph::{DecodeLimits, GraphRefusal};
use lash_durable::domain::graph_commit::{check_graph, measure_owner_commit};
use lash_durable::domain::pages::CandidatePublication;
use lash_durable::domain::{
    ExecKey, ExecRootRow, HydratedGraph, OwnerKey, ProcessActorRow, RootOwner, RunFacts, ScopeKey,
    SessionCloseRow, TurnRow, WaitId, WaitRow,
};
use lash_durable::{
    ActorCommit, ActorKey, ActorSnapshot, ActorState, ActorTx, BootId, ClaimCause, ClaimPurpose,
    Claimed, CommitLabel, DomainWrite, DurableError, DurableInstant, DurableReads, DurableStore,
    Epoch, Fenced, HeartbeatOutcome, Mail, MailAnswer, MailCommit, MailDomainWrite, MailKind,
    MailRefusal, MailSeq, MailTx, MailWrite, NodeId, NodeLease, NodeSpec, OpenedActor, Owner,
    Reaped, Release, StateRevision, StoreFailure, StoreFailureKind, Woken,
};
use lash_store_sql::durable::processes::{ActorParkStatements, ProcessActorStatements};
use lash_store_sql::durable::session_mail::SessionMailStatements;
use lash_store_sql::durable::{ActorStatements, MailStatements, NodeStatements};
use rusqlite::{Connection, OptionalExtension};

use crate::conn::{FencedTx, SqliteConnection, TxOutcome, cached_execute};
use crate::liveness_locks::{LivenessLocks, Probed};

mod access;
mod claim_facts;
pub(crate) mod environment;
mod graph;
mod model_calls;
mod node_wake_sql;
pub(crate) mod packages;
mod pages;
mod park;
mod process_view;
pub(crate) mod processes;
mod prompts;

/// Drain one bounded page; the collection repeats pages in separate transactions.
///
/// A page releases settled prompt audits and, at most
/// [`MATERIAL_RELEASE_BATCH`](lash_store_sql::session::revisions::MATERIAL_RELEASE_BATCH)
/// each, the bodies of settled inputs and the payloads of applied commands
/// whose revision is no longer retained (FIG-6607), and settled host
/// tool-intent payloads whose owning run's revision is released (FIG-6612).
pub(crate) fn release_material_batch(
    tx: &Connection,
    session: Option<&lash_kernel_value::identity::SessionId>,
) -> rusqlite::Result<usize> {
    let mut released = prompts::release_unretained(tx, session)?;
    let limit = i64::from(lash_store_sql::session::revisions::MATERIAL_RELEASE_BATCH);
    let ingress = crate::turn_ingress::turn_ingress_sql();
    for statement in [
        &ingress.pending_inputs.release_settled_bodies,
        &ingress.queued_batches.release_settled_payloads,
    ] {
        released += cached_execute(
            tx,
            statement.sql(),
            rusqlite::params![
                session.map(lash_kernel_value::identity::SessionId::as_str),
                limit
            ],
        )?;
    }
    // A released body no longer holds its attachments (FIG-6611).
    let holds = &crate::attachments::attachment_sql().holds;
    for statement in [&holds.queue_released_ingress, &holds.end_released_ingress] {
        cached_execute(
            tx,
            statement.sql(),
            rusqlite::params![session.map(lash_kernel_value::identity::SessionId::as_str)],
        )?;
    }
    released += crate::turn_ingress::release_settled_tool_intent_payloads(tx, session)?;
    Ok(released)
}
#[path = "../durable_reads.rs"]
mod reads;
mod run_records;
mod session_close;
pub(crate) mod session_mail;
#[cfg(any(test, feature = "testing"))]
pub(crate) use session_mail::cut_session_wakes;
mod snapshots;
mod turns;
mod waits;

#[path = "../node_wakes.rs"]
mod node_wakes;

pub(super) use node_wake_sql::{NODE_WAKE_CAPTURE, NODE_WAKE_PEERS};
#[cfg(test)]
pub(super) use node_wake_sql::{
    NODE_WAKE_FINAL_OWNER, NODE_WAKE_OWNED_SETUP, NODE_WAKE_ROLLBACK_MAIL,
    NODE_WAKE_SESSION_REVISION,
};
pub(crate) use node_wakes::TABLES as NODE_WAKES_TABLES;
pub(crate) use node_wakes::{Capture, NodeWakeWriter, SqliteNodeWakes};

// The domain tables I0 creates in the durable core database (FIG-5194);
// each domain's own lane adds its tables beside them.
pub(crate) use graph::TABLES as GRAPH_TABLES;
pub(crate) use graph::{
    AUDIT_PAGE, GraphClosure, audit_page, config_root_visible, graph_closure, graph_counts,
    install_staged, peel, retire_roots,
};
pub(crate) use model_calls::TABLES as MODEL_CALL_TABLES;
#[cfg(feature = "testing")]
pub(crate) use pages::cut_retirement_holds;
pub(crate) use pages::retire_step_in as retire_step;
pub(crate) use prompts::TABLES as PROMPT_SNAPSHOTS_TABLES;
pub(crate) use prompts::release as release_prompt_snapshots;
pub(crate) use run_records::TABLES as RUN_RECORDS_TABLES;
pub(crate) use session_close::TABLES as SESSION_CLOSE_TABLES;
pub(crate) use snapshots::TABLES as EXEC_SNAPSHOTS_TABLES;
pub(crate) use turns::TABLES as TURN_PHASES_TABLES;
pub(crate) use waits::TABLES as WAITS_TABLES;

/// The engine's tables, carried by the durable core.
/// Delete every engine row `session` owns, in its physical delete: its
/// turns' run facts (their tool-call ledgers), its cells' roots, admissions
/// and facts, the waits its actor owns and its turns' wait tombstones. The
/// session's actor row stays, its terminal fence. Answers every graph root
/// those rows held: the delete's to retire.
pub(crate) fn retire_session_rows(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
) -> rusqlite::Result<Vec<String>> {
    use lash_durable::domain::{ExecKey, OwnerKey, ScopeKey};

    let mut roots = environment::remove_head(tx, session)?;
    roots.extend(run_records::delete_under(
        tx,
        &OwnerKey::session_turns(session),
    )?);
    roots.extend(model_calls::delete_session(tx, session)?);
    roots.extend(snapshots::retire_under(
        tx,
        &ExecKey::session_cells(session),
    )?);
    if let Ok(actor) = ActorKey::session(session.as_str()) {
        waits::delete_owned(tx, &actor)?;
    }
    waits::delete_under(tx, &ScopeKey::session_turns(session))?;
    Ok(roots)
}

/// Delete `process`'s run facts and the waits it owns, in its prune: the
/// process host API's tool calls go with its record, and the RoundFact
/// nodes they held are peeled.
pub(crate) fn retire_process_rows(
    tx: &Connection,
    process: &lash_vocabulary::identity::ProcessId,
) -> rusqlite::Result<()> {
    let mut facts = run_records::delete_owner(
        tx,
        &lash_durable::domain::OwnerKey::Process(process.clone()).stored(),
    )?;
    facts.extend(model_calls::delete_session(
        tx,
        &lash_kernel_value::identity::SessionId::prefixed("process:", process.as_str()),
    )?);
    if let Ok(actor) = ActorKey::process(process.as_str()) {
        waits::delete_owned(tx, &actor)?;
    }
    graph::peel(tx, facts.into_iter().collect()).map(drop)
}

/// Every per-owner engine row in `conn`'s database, one rendered line each:
/// run facts, execution roots, effect admissions, waits (whole or
/// tombstone), turn cancel requests and actors. What a law reads to show
/// an owner's rows are gone (FIG-5939).
#[cfg(feature = "testing")]
pub(crate) fn engine_rows_for_testing(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut rows = Vec::new();
    for sql in [
        "SELECT 'run_records ' || owner_key || ' ' || run_seq || '/' || ordinal || ' ' || lower(hex(record_ref))
             FROM run_records",
        "SELECT 'exec_snapshots ' || exec_key FROM exec_snapshots",
        "SELECT 'effect_admissions ' || exec_key FROM effect_admissions",
        "SELECT 'waits ' || owner_scope || ' ' || COALESCE(owner_actor, '-') || ' ' || state
             || CASE WHEN compacted THEN ' tombstone' ELSE ' whole' END FROM waits",
        "SELECT 'turn_cancel_requests ' || session_id || ' ' || turn_id FROM turn_cancel_requests",
        "SELECT 'actors ' || actor_key || ' ' || state FROM actors",
    ] {
        let mut statement = conn.prepare(sql)?;
        let read = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.extend(read);
    }
    rows.sort();
    Ok(rows)
}

/// The leases held under `actor`'s fence, for external activation laws.
#[cfg(feature = "testing")]
pub(crate) fn root_leases_for_testing(
    conn: &Connection,
    actor: &ActorKey,
) -> rusqlite::Result<Vec<(lash_durable::domain::pages::RootLeaseId, DurableInstant)>> {
    let mut statement = conn.prepare(
        "SELECT lease_id, expires_at_ms FROM root_leases WHERE actor_key = ?1 ORDER BY lease_id",
    )?;
    statement
        .query_map([actor.as_str()], |row| {
            Ok((
                lash_durable::domain::pages::RootLeaseId(row.get::<_, i64>(0)?.cast_unsigned()),
                DurableInstant(row.get(1)?),
            ))
        })?
        .collect()
}

mod schema;
pub(crate) use schema::DURABLE_TABLES;

use access::SqliteDurableStatements;

pub(crate) struct Sql {
    node: NodeStatements,
    pub(crate) actor: ActorStatements,
    mail: MailStatements,
    park: ActorParkStatements,
    process: ProcessActorStatements,
    session_mail: SessionMailStatements,
    sqlite: SqliteDurableStatements,
}

pub(crate) static SQL: LazyLock<Sql> = LazyLock::new(|| {
    let dialect = crate::schema_layout::MAIN;
    Sql {
        node: NodeStatements::render(dialect),
        actor: ActorStatements::render(dialect),
        mail: MailStatements::render(dialect),
        park: ActorParkStatements::render(dialect),
        process: ProcessActorStatements::render(dialect),
        session_mail: SessionMailStatements::render(
            dialect.with_vocabulary(crate::turn_ingress::TURN_INPUT_LIFECYCLE),
        ),
        sqlite: SqliteDurableStatements::render(dialect),
    }
});

/// The [`DurableStore`] over a SQLite store set's durable core.
#[derive(Clone)]
pub struct SqliteDurableStore {
    graph_reads: lash_core_store::checkpoint_graph::GraphReadPolicy,
    conn: SqliteConnection,
    /// Where the engine's reads run, off the writer's thread.
    readers: crate::conn::ReadPool,
    clock: Arc<dyn Clock>,
    blob_profile: crate::BuiltinBlobProfile,
    observer: lash_core_ids::operational_metrics::StoreObserver,
}

impl std::fmt::Debug for SqliteDurableStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteDurableStore").finish_non_exhaustive()
    }
}

/// A transaction's answer: committed with its value, or refused typed and
/// rolled back.
type Flow<T> = rusqlite::Result<TxOutcome<Result<T, DurableError>>>;

/// A statement's answer inside a transaction: a value, or a typed refusal
/// that rolls the transaction back.
pub(crate) type Answer<T> = rusqlite::Result<Result<T, DurableError>>;

/// The owner commit a domain write is applied in: after its fence.
pub(crate) struct Committing<'a> {
    pub(crate) process_views: lash_durable::domain::processes::ProcessPostimages,
    /// The actor whose fence matched.
    pub(crate) actor: &'a ActorKey,
    /// The epoch it matched at; domain rows record it as `written_epoch`.
    pub(crate) epoch: Epoch,
    /// The transaction's one clock reading.
    pub(crate) now: DurableInstant,
    /// The fleet format the transaction's fence read: registry rows a
    /// domain write touches are encoded under it.
    pub(crate) fleet: lash_core_store::store::FleetFormat,
    /// How the session store a head commit writes keeps its blobs.
    pub(crate) blob_profile: crate::BuiltinBlobProfile,
    /// The roots its writes retired, moved to retirement holds before it
    /// commits.
    pub(crate) retired: Retired,
    /// The open turn's phase roots its fenced turn read saw.
    pub(crate) open_roots: turns::OpenRoots,
}

/// The roots a transaction's writes retired: what it moves to retirement
/// holds before it commits (FIG-5949, D-RET). A turn's terminal retires its
/// PhaseRoot alone: the TurnContent under it is the transcript the
/// session's next turn names again, so its hold peels the PhaseRoot only
/// and the content waits for the orphan audit rather than being
/// reinstalled (the TurnContent ordering constraint).
#[derive(Default)]
pub(crate) struct Retired {
    closures: std::cell::RefCell<std::collections::BTreeSet<String>>,
    phase_ends: std::cell::RefCell<std::collections::BTreeSet<String>>,
}

impl Retired {
    /// `root` was retired: hold its closure.
    pub(crate) fn closure(&self, root: &lash_core_store::checkpoint_graph::NodeId) {
        self.closures.borrow_mut().insert(root.to_hex());
    }

    /// A stored root reference was retired: hold its closure.
    pub(crate) fn stored(&self, root: impl Into<String>) {
        self.closures.borrow_mut().insert(root.into());
    }

    /// An ended turn's PhaseRoot was retired: hold it alone.
    pub(crate) fn phase_end(&self, root: impl Into<String>) {
        self.phase_ends.borrow_mut().insert(root.into());
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        std::collections::BTreeSet<String>,
        std::collections::BTreeSet<String>,
    ) {
        (self.closures.into_inner(), self.phase_ends.into_inner())
    }
}

fn refuse<T>(error: DurableError) -> Flow<T> {
    Ok(TxOutcome::Rollback(Err(error)))
}

fn commit<T>(value: T) -> Flow<T> {
    Ok(TxOutcome::Commit(Ok(value)))
}

impl SqliteDurableStore {
    pub(crate) fn new(
        conn: SqliteConnection,
        readers: crate::conn::ReadPool,
        clock: Arc<dyn Clock>,
        blob_profile: crate::BuiltinBlobProfile,
        observer: lash_core_ids::operational_metrics::StoreObserver,
        graph_reads: lash_core_store::checkpoint_graph::GraphReadPolicy,
    ) -> Self {
        Self {
            conn,
            readers,
            clock,
            blob_profile,
            observer,
            graph_reads,
        }
    }

    /// Every registered boot, and whether some listener holds its liveness
    /// lock in `locks`, each probed inside the one read.
    async fn liveness(
        &self,
        locks: LivenessLocks,
    ) -> Result<Vec<lash_durable::BootLiveness>, DurableError> {
        self.read(move |tx| {
            let boots = registered_boots(tx)?;
            let mut liveness = Vec::with_capacity(boots.len());
            for boot in boots {
                let held = match locks.probe(&node_wakes::boot_lock(&boot.boot)) {
                    Ok(Probed::Held) => true,
                    Ok(Probed::Free(_)) => false,
                    Err(error) => return Ok(Err(node_wakes::lock_failure(&error))),
                };
                liveness.push(lash_durable::BootLiveness { boot, held });
            }
            Ok(Ok(liveness))
        })
        .await
    }

    /// Reap `boot` when its liveness lock in `locks` is free and the
    /// reaper's own is held, in one transaction under the reap's label. The
    /// free lock is kept shared until the reap commits, so the boot cannot
    /// lock again before it; a boot that locks after finds its lease gone.
    /// Its lock file is deleted once the reap commits.
    async fn reap_released(
        &self,
        locks: LivenessLocks,
        reaper: &NodeLease,
        boot: &Owner,
    ) -> Result<Vec<Reaped>, DurableError> {
        let reaper = reaper.owner.clone();
        let boot = boot.clone();
        let (reaped, released) = self
            .write(CommitLabel::REAP, move |tx, now| {
                if !node_live(tx, &reaper)? {
                    return refuse(DurableError::NodeLeaseLost {
                        node: reaper.node.clone(),
                    });
                }
                match locks.probe(&node_wakes::boot_lock(&reaper.boot)) {
                    Ok(Probed::Held) => {}
                    Ok(Probed::Free(_)) => return commit((Vec::new(), None)),
                    Err(error) => return refuse(node_wakes::lock_failure(&error)),
                }
                let released = match locks.probe(&node_wakes::boot_lock(&boot.boot)) {
                    Ok(Probed::Free(released)) => released,
                    Ok(Probed::Held) => return commit((Vec::new(), None)),
                    Err(error) => return refuse(node_wakes::lock_failure(&error)),
                };
                let deleted = tx
                    .prepare_cached(SQL.node.delete_boot.sql())?
                    .query_row([boot.node.as_str(), boot.boot.as_str()], |_| Ok(()))
                    .optional()?;
                if deleted.is_none() {
                    return commit((Vec::new(), released));
                }
                match release_owned_by(tx, &boot, now)? {
                    Ok(owned) => commit((
                        owned
                            .into_iter()
                            .map(|(actor, epoch)| Reaped {
                                actor,
                                from: boot.clone(),
                                epoch,
                            })
                            .collect(),
                        released,
                    )),
                    Err(error) => refuse(error),
                }
            })
            .await?;
        if let Some(released) = released {
            released.delete();
        }
        Ok(reaped)
    }
}

/// Every registered boot, oldest node first.
fn registered_boots(tx: &Connection) -> rusqlite::Result<Vec<Owner>> {
    tx.prepare_cached(SQL.sqlite.boots.sql())?
        .query_map([], |row| {
            Ok(Owner {
                node: NodeId::new(row.get::<_, String>(0)?),
                boot: BootId::new(row.get::<_, String>(1)?),
            })
        })?
        .collect()
}

fn store_failure(error: rusqlite::Error) -> DurableError {
    use lash_core_store::store::StoreError;
    let kind = match &error {
        rusqlite::Error::FromSqlConversionFailure(..)
        | rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::IntegralValueOutOfRange(..) => StoreFailureKind::Corrupt,
        _ => StoreFailureKind::Unavailable,
    };
    let error = crate::sqlite_error(error);
    let kind = match &error {
        StoreError::Contended => StoreFailureKind::Contended,
        StoreError::WriterFenced { .. } => StoreFailureKind::WriterRetired,
        // Bytes a write's own read cannot decode never clear by themselves.
        StoreError::StoredDataCorrupt { .. } => StoreFailureKind::Corrupt,
        _ => kind,
    };
    DurableError::Store(StoreFailure {
        code: error.storage_code().cloned(),
        kind,
        message: error.to_string(),
    })
}

/// Checked SQL/domain integers: foreign out-of-range data is corruption.
fn integer<T>(
    value: impl TryInto<T, Error: std::error::Error + Send + Sync + 'static>,
) -> rusqlite::Result<T> {
    value.try_into().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn corrupt(what: &str, value: &str) -> DurableError {
    DurableError::Store(StoreFailure {
        code: None,
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

fn actor_state(stored: &str) -> Result<ActorState, DurableError> {
    ActorState::parse(stored).ok_or_else(|| corrupt("actor state", stored))
}

fn formats_json(spec: &[lash_durable::FormatSet]) -> String {
    let formats: Vec<&str> = spec.iter().map(lash_durable::FormatSet::as_str).collect();
    serde_json::Value::from(formats).to_string()
}

fn owner(node: Option<String>, boot: Option<String>) -> Option<Owner> {
    Some(Owner {
        node: NodeId::new(node?),
        boot: BootId::new(boot?),
    })
}

/// Release every actor `owner` holds as ready at `now`, epochs bumped.
fn release_owned_by(
    tx: &Connection,
    owner: &Owner,
    now: DurableInstant,
) -> rusqlite::Result<Result<Vec<(ActorKey, Epoch)>, DurableError>> {
    let mut statement = tx.prepare_cached(SQL.actor.release_owned_by.sql())?;
    let rows = statement
        .query_map(
            rusqlite::params![owner.node.as_str(), owner.boot.as_str(), now.0],
            |row| Ok((row.get::<_, String>(0)?, Epoch(row.get(1)?))),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|(key, epoch)| actor_key(&key).map(|key| (key, epoch)))
        .collect())
}

/// Classify a refused owner fence from the same writer transaction: loss
/// of the grant or a revision that moved within the held epoch.
fn fenced(
    tx: &Connection,
    actor: &ActorKey,
    held: Epoch,
    expected: StateRevision,
) -> rusqlite::Result<DurableError> {
    let current = tx
        .prepare_cached(SQL.actor.epoch_of.sql())?
        .query_row([actor.as_str()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .optional()?;
    if let Some((epoch, state, actual)) = &current {
        let state = match actor_state(state) {
            Ok(state) => state,
            Err(error) => return Ok(error),
        };
        if *epoch == held.0 && state == ActorState::Owned && *actual != expected.0 {
            return Ok(DurableError::RevisionConflict {
                actor: actor.clone(),
                epoch: held,
                expected,
                actual: StateRevision(*actual),
            });
        }
    }
    Ok(DurableError::OwnershipLost(Fenced {
        actor: actor.clone(),
        held,
        current: current.map(|(epoch, _, _)| Epoch(epoch)),
    }))
}

fn node_live(tx: &Connection, node: &Owner) -> rusqlite::Result<bool> {
    node_draining(tx, node).map(|draining| draining.is_some())
}

/// Whether `node` is draining, or `None` when it holds no lease.
fn node_draining(tx: &Connection, node: &Owner) -> rusqlite::Result<Option<bool>> {
    tx.prepare_cached(SQL.sqlite.node_live.sql())?
        .query_row([node.node.as_str(), node.boot.as_str()], |row| {
            row.get::<_, bool>(0)
        })
        .optional()
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
pub(crate) fn wake_within(
    tx: &Connection,
    actor: &ActorKey,
    control: bool,
    now: DurableInstant,
) -> Answer<(Woken, MailSeq)> {
    let statement = if control {
        SQL.park.control_wake.sql()
    } else {
        SQL.actor.wake.sql()
    };
    let woke = tx
        .prepare_cached(statement)?
        .query_row(rusqlite::params![actor.as_str(), now.0], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .optional()?;
    let Some((seq, state, node, boot)) = woke else {
        return Ok(Err(DurableError::MailRefused(refusal_for_unwakeable(
            tx, actor,
        )?)));
    };
    Ok(actor_state(&state).map(|state| {
        (
            Woken {
                actor: actor.clone(),
                state,
                owner: owner(node, boot),
            },
            MailSeq(seq),
        )
    }))
}

/// Guard and advance a fresh direct session mutation in its writer transaction.
pub(crate) fn bump_unclaimed_session(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
    at_ms: u64,
) -> Result<(), lash_core_store::store::StoreError> {
    use lash_core_store::store::{HeadOwnershipFacts, StoreError, require_unclaimed_actor};
    let actor = ActorKey::session(session.as_str())
        .map_err(|error| StoreError::Backend(error.to_string()))?;
    cached_execute(
        tx,
        SQL.actor.create_idle.sql(),
        rusqlite::params![
            actor.as_str(),
            lash_durable::domain::SESSION_ACTOR_FORMATS,
            crate::clamp_epoch_ms(at_ms),
        ],
    )
    .map_err(crate::sqlite_error)?;
    let claimed_epoch = tx
        .query_row(SQL.actor.claimed_epoch.sql(), [actor.as_str()], |row| {
            row.get(0)
        })
        .optional()
        .map_err(crate::sqlite_error)?;
    require_unclaimed_actor(
        session,
        &HeadOwnershipFacts {
            claimed_epoch,
            ..Default::default()
        },
    )?;
    let bumped = tx
        .query_row(SQL.actor.bump_unclaimed.sql(), [actor.as_str()], |row| {
            row.get::<_, i64>(0)
        })
        .optional()
        .map_err(crate::sqlite_error)?;
    if bumped.is_none() {
        return Err(StoreError::SessionDeleted {
            session_id: session.clone(),
        });
    }
    Ok(())
}

/// Wake session `session`'s actor inside the caller's transaction, creating
/// it first when this is its first work: the one call every producer of
/// session work makes beside its row (L3s, FIG-5196). The caller has
/// already refused an absent or deleted session, so the actor it creates
/// belongs to a live one. `control` marks a cancel.
pub(crate) fn wake_session_within(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
    control: bool,
    now: DurableInstant,
) -> Answer<Woken> {
    let actor = match ActorKey::session(session.as_str()) {
        Ok(actor) => actor,
        Err(error) => return Ok(Err(corrupt("session actor key", &error.to_string()))),
    };
    tx.prepare_cached(SQL.actor.create.sql())?
        .query_row(
            rusqlite::params![
                actor.as_str(),
                actor.kind().as_str(),
                lash_durable::domain::SESSION_ACTOR_FORMATS,
                now.0
            ],
            |_| Ok(()),
        )
        .optional()?;
    Ok(wake_within(tx, &actor, control, now)?.map(|(woken, _)| woken))
}

/// [`wake_session_within`] for a session store transaction at `at_ms`,
/// answering the actor it woke for the caller to hand to the store set's
/// wake sink once the transaction commits. An absent or deleted session,
/// and one whose actor already ended (its close finished), wakes nobody and
/// creates nothing; that is no refusal of the producer's write, which
/// refuses such a session itself.
pub(crate) fn wake_session_tx(
    tx: &Connection,
    session: &lash_kernel_value::identity::SessionId,
    control: bool,
    at_ms: u64,
) -> Result<Option<Woken>, lash_core_store::store::StoreError> {
    let (meta, deleted): (i64, i64) = tx
        .prepare_cached(SQL.session_mail.standing.sql())
        .and_then(|mut statement| {
            statement.query_row([session.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))
        })
        .map_err(crate::sqlite_error)?;
    if meta == 0 || deleted > 0 {
        return Ok(None);
    }
    let now = DurableInstant(integer::<i64>(at_ms).map_err(crate::sqlite_error)?);
    match wake_session_within(tx, session, control, now).map_err(crate::sqlite_error)? {
        Ok(woken) => Ok(Some(woken)),
        Err(DurableError::MailRefused(MailRefusal::ActorTerminal(_))) => Ok(None),
        Err(error) => Err(lash_core_store::store::StoreError::Backend(format!(
            "session {session} was not woken: {error}"
        ))),
    }
}

/// Apply one owner-commit domain write by its domain's module.
fn apply_domain(
    tx: &FencedTx<'_>,
    committing: &Committing<'_>,
    write: &DomainWrite,
    sessions: &owner::PreparedSessions,
    planned_under: lash_core_store::store::FleetFormat,
) -> Answer<()> {
    match write {
        DomainWrite::AdoptExecution(record) => packages::adopt(tx, committing.actor, record),
        DomainWrite::SeedResolution(record) => packages::seed(tx, committing.actor, record),
        DomainWrite::EndExecution(execution) => {
            packages::end_execution(tx, committing.actor, execution)
        }
        DomainWrite::RegisterExecution(write) => packages::register(tx, committing.actor, write),
        DomainWrite::Environment(write) => environment::apply(tx, committing, write),
        DomainWrite::Turn(write) => turns::apply(tx, committing, write),
        DomainWrite::SessionCommit(write) => turns::apply_session_commit(
            tx,
            committing,
            write,
            sessions
                .get(&(write.session.clone(), write.expected_head))
                .and_then(|contents| contents.get(write.commit_json.as_str()))
                .ok_or_else(|| {
                    crate::sqlite_conversion_error(crate::stored_data_corrupt(
                        "prepared owner commit",
                        "missing exact session/head/content",
                    ))
                })?,
            planned_under,
        )
        .map(|result| result.map(drop)),
        DomainWrite::RunFact(write) => run_records::apply(tx, committing, write),
        DomainWrite::ExecRoot(write) => snapshots::apply(tx, committing, write),
        DomainWrite::EffectAdmission(write) => graph::apply_admission(tx, committing, write),
        DomainWrite::Wait(write) => waits::apply(tx, committing, write),
        DomainWrite::Process(write) => processes::apply(tx, committing, write),
        DomainWrite::SessionClose(write) => session_close::apply(tx, committing, write),
        DomainWrite::ActorPark { reason_json } => {
            park::set(tx, committing.actor.as_str(), reason_json)
        }
        DomainWrite::SessionMail(write) => session_mail::apply(tx, committing, write),
        DomainWrite::Prompt(write) => prompts::apply(tx, committing, write),
        DomainWrite::ModelCallPin(write) => model_calls::apply_pin(tx, write),
        DomainWrite::ModelCall(write) => model_calls::apply(tx, committing, write),
    }
}

/// Apply one mailbox domain write by its domain's module, with its answer
/// and the actors it woke.
fn apply_mail_domain(
    tx: &Connection,
    write: &MailDomainWrite,
    now: DurableInstant,
    fleet: lash_core_store::store::FleetFormat,
) -> Answer<(MailAnswer, Vec<Woken>)> {
    Ok(match write {
        MailDomainWrite::ResolveWait(resolution) => waits::resolve(tx, resolution, now)?
            .map(|(answer, woken)| (MailAnswer::ResolveWait(answer), woken.into_iter().collect())),
        MailDomainWrite::RequestProcessCancel(request) => {
            processes::request_cancel(tx, request, now, fleet)?.map(|(answer, woken)| {
                (
                    MailAnswer::RequestProcessCancel(answer),
                    woken.into_iter().collect(),
                )
            })
        }
        MailDomainWrite::RequestToolCallCancel(request) => {
            run_records::request_call_cancel(tx, request, now)?.map(|(answer, woken)| {
                (
                    MailAnswer::RequestToolCallCancel(answer),
                    woken.into_iter().collect(),
                )
            })
        }
        MailDomainWrite::RequestTurnCancel(request) => turns::request_cancel(tx, request, now)?
            .map(|(answer, woken)| {
                (
                    MailAnswer::RequestTurnCancel(answer),
                    woken.into_iter().collect(),
                )
            }),
        MailDomainWrite::RequestOperationCancel(request) => {
            session_mail::request_operation_cancel(tx, request, now)?.map(|(answer, woken)| {
                (
                    MailAnswer::RequestOperationCancel(answer),
                    woken.into_iter().collect(),
                )
            })
        }
        MailDomainWrite::Redrive(request) => park::redrive(tx, request, now)?
            .map(|(answer, woken)| (MailAnswer::Redrive(answer), woken.into_iter().collect())),
        MailDomainWrite::RedriveSessionParks(redrive) => {
            park::redrive_session_parks(tx, redrive, now)?
                .map(|(redriven, woken)| (MailAnswer::RedriveSessionParks(redriven), woken))
        }
    })
}

fn refusal_for_unwakeable(tx: &Connection, actor: &ActorKey) -> rusqlite::Result<MailRefusal> {
    let state = tx
        .prepare_cached(SQL.actor.epoch_of.sql())?
        .query_row([actor.as_str()], |row| row.get::<_, String>(1))
        .optional()?;
    Ok(match state {
        None => MailRefusal::UnknownActor(actor.clone()),
        Some(_) => MailRefusal::ActorTerminal(actor.clone()),
    })
}

fn note_woken(woken: &mut Vec<Woken>, entry: Woken) {
    match woken.iter_mut().find(|seen| seen.actor == entry.actor) {
        Some(seen) => *seen = entry,
        None => woken.push(entry),
    }
}

fn apply_mail(tx: &FencedTx<'_>, writes: MailTx, now: DurableInstant) -> Flow<MailCommit> {
    let fleet = tx.fleet();
    let tx: &Connection = tx;
    let mut receipt = MailCommit::default();
    for write in writes.writes() {
        match write {
            MailWrite::CreateActor { actor, formats } => {
                let created = tx
                    .prepare_cached(SQL.actor.create.sql())?
                    .query_row(
                        rusqlite::params![
                            actor.as_str(),
                            actor.kind().as_str(),
                            formats.as_str(),
                            now.0
                        ],
                        |_| Ok(()),
                    )
                    .optional()?;
                if created.is_none() {
                    return refuse(DurableError::MailRefused(MailRefusal::ActorExists(
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
                let woken = match wake_within(tx, actor, write.controls(), now)? {
                    Ok(woken) => woken,
                    Err(error) => return refuse(error),
                };
                if let MailWrite::Append { kind, body, .. } = write {
                    let seq = woken.1;
                    cached_execute(
                        tx,
                        SQL.mail.append.sql(),
                        rusqlite::params![actor.as_str(), seq.0, kind.as_str(), body, now.0],
                    )?;
                    receipt.appended.push((actor.clone(), seq));
                }
                note_woken(&mut receipt.woken, woken.0);
            }
            MailWrite::Domain(domain) => {
                let (answer, woken) = match apply_mail_domain(tx, domain, now, fleet)? {
                    Ok(applied) => applied,
                    Err(error) => return refuse(error),
                };
                receipt.answers.push(answer);
                for woken in woken {
                    note_woken(&mut receipt.woken, woken);
                }
            }
        }
    }
    commit(receipt)
}

#[async_trait::async_trait]
impl DurableStore for SqliteDurableStore {
    fn environments(&self) -> Arc<dyn lash_durable::environment::EnvironmentStore> {
        Arc::new(self.clone())
    }
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.instant()
    }

    fn pages(&self) -> Option<std::sync::Arc<dyn lash_durable::domain::pages::PageStore>> {
        Some(std::sync::Arc::new(self.clone()))
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        let lease_owner = Owner {
            node: spec.node.clone(),
            boot: BootId::new(uuid::Uuid::new_v4().to_string()),
        };
        let formats = formats_json(&spec.decodes);
        let capabilities = lash_durable::packages::encode_capabilities(&spec.capabilities);
        let spec = spec.clone();
        let answer = self
            .write(CommitLabel::NODE_REGISTER, move |tx, now| {
                let boots = tx
                    .prepare_cached(SQL.node.delete_boots.sql())?
                    .query_map([spec.node.as_str()], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for boot in boots {
                    let earlier = Owner {
                        node: spec.node.clone(),
                        boot: BootId::new(boot),
                    };
                    if let Err(error) = release_owned_by(tx, &earlier, now)? {
                        return refuse(error);
                    }
                }
                let expires_at = now.after_millis(spec.ttl_millis);
                cached_execute(
                    tx,
                    SQL.node.insert.sql(),
                    rusqlite::params![
                        lease_owner.node.as_str(),
                        lease_owner.boot.as_str(),
                        formats,
                        now.0,
                        expires_at.0,
                        capabilities
                    ],
                )?;
                commit(NodeLease {
                    owner: lease_owner,
                    serves: spec.served().into(),
                    decodes: spec.decodes,
                    capabilities: spec.capabilities.clone().into(),
                    ttl_millis: spec.ttl_millis,
                    expires_at,
                })
            })
            .await;
        #[cfg(feature = "perf-witness")]
        if answer.is_ok() {
            lash_core_ids::perf_witness::startup::record(
                lash_core_ids::perf_witness::startup::Phase::NodeRegistered,
            );
        }
        answer
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        let owner = node.owner.clone();
        let ttl = node.ttl_millis;
        self.write(CommitLabel::HEARTBEAT, move |tx, now| {
            let renewed = tx
                .prepare_cached(SQL.node.renew.sql())?
                .query_row(
                    rusqlite::params![
                        owner.node.as_str(),
                        owner.boot.as_str(),
                        now.after_millis(ttl).0
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            commit(match renewed {
                Some(expires_at) => HeartbeatOutcome::Renewed {
                    expires_at: DurableInstant(expires_at),
                },
                None => HeartbeatOutcome::Reaped,
            })
        })
        .await
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        let reaper = reaper.owner.clone();
        self.write(CommitLabel::REAP, move |tx, now| {
            if !node_live(tx, &reaper)? {
                return refuse(DurableError::NodeLeaseLost {
                    node: reaper.node.clone(),
                });
            }
            let dead = tx
                .prepare_cached(SQL.node.delete_expired.sql())?
                .query_map(
                    rusqlite::params![now.0, reaper.node.as_str(), reaper.boot.as_str()],
                    |row| {
                        Ok(Owner {
                            node: NodeId::new(row.get::<_, String>(0)?),
                            boot: BootId::new(row.get::<_, String>(1)?),
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if dead.is_empty() {
                return Ok(TxOutcome::Rollback(Ok(Vec::new())));
            }
            let mut reaped = Vec::new();
            for from in dead {
                match release_owned_by(tx, &from, now)? {
                    Ok(released) => {
                        reaped.extend(released.into_iter().map(|(actor, epoch)| Reaped {
                            actor,
                            from: from.clone(),
                            epoch,
                        }))
                    }
                    Err(error) => return refuse(error),
                }
            }
            commit(reaped)
        })
        .await
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        let owner = node.owner.clone();
        self.write(CommitLabel::NODE_RELEASE, move |tx, now| {
            let deleted = tx
                .prepare_cached(SQL.node.delete_boot.sql())?
                .query_row([owner.node.as_str(), owner.boot.as_str()], |_| Ok(()))
                .optional()?;
            if deleted.is_none() {
                return refuse(DurableError::NodeLeaseLost {
                    node: owner.node.clone(),
                });
            }
            match release_owned_by(tx, &owner, now)? {
                Ok(released) => commit(released.into_iter().map(|(actor, _)| actor).collect()),
                Err(error) => refuse(error),
            }
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
        let owner = node.owner.clone();
        let limit = integer::<i64>(limit).map_err(store_failure)?;
        let lease = node.clone();
        self.write(CommitLabel::CLAIM, move |tx, now| {
            match node_draining(tx, &owner)? {
                None => {
                    return refuse(DurableError::NodeLeaseLost {
                        node: owner.node.clone(),
                    });
                }
                Some(true) => return commit(Vec::new()),
                Some(false) => {}
            }
            match processes::claim_within(tx, &lease, now, limit)? {
                Ok(claimed) => commit(claimed),
                Err(error) => refuse(error),
            }
        })
        .await
    }

    async fn readiness(&self) -> Result<lash_durable::Readiness, DurableError> {
        self.readiness_now().await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        let owner = node.owner.clone();
        let lease = node.clone();
        self.read(move |tx| {
            let rows = tx
                .prepare_cached(SQL.actor.owned_by.sql())?
                .query_map([owner.node.as_str(), owner.boot.as_str()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows
                .into_iter()
                .map(|(key, epoch, stored_formats, requires)| {
                    Ok(Claimed {
                        actor: actor_key(&key)?,
                        epoch: Epoch(epoch),
                        cause: ClaimCause::Adopted,
                        purpose: ClaimPurpose::of_stored(
                            &lease,
                            &stored_formats,
                            requires.as_deref(),
                        )?,
                        // `of_stored` refused a corrupt spelling above.
                        requires: lash_durable::parse_requires(requires.as_deref())
                            .unwrap_or_default(),
                    })
                })
                .collect())
        })
        .await
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        let owner = node.owner.clone();
        self.write(CommitLabel::NODE_DRAIN, move |tx, _now| {
            let marked = tx
                .prepare_cached(SQL.node.mark_draining.sql())?
                .query_row([owner.node.as_str(), owner.boot.as_str()], |_| Ok(()))
                .optional()?;
            match marked {
                Some(()) => commit(()),
                None => refuse(DurableError::NodeLeaseLost {
                    node: owner.node.clone(),
                }),
            }
        })
        .await
    }

    async fn live_nodes(&self) -> Result<Vec<lash_durable::NodeCapability>, DurableError> {
        let now = self.instant()?;
        self.read(move |tx| packages::live_nodes(tx, now)).await
    }

    async fn actor_requirements(
        &self,
        after: Option<&lash_durable::ActorKey>,
        limit: usize,
    ) -> Result<Vec<(lash_durable::ActorKey, Vec<String>)>, DurableError> {
        let after = after.map(lash_durable::ActorKey::to_string);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.read(move |tx| packages::actor_requirements(tx, after.as_deref(), limit))
            .await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        lash_core_ids::operational_metrics::sql::owner_state_read(
            lash_core_ids::operational_metrics::sql::OwnerStateRead::Phase,
        );
        let actor = actor.clone();
        let at = self.instant()?;
        let session = match actor.kind() {
            lash_durable::ActorKind::Session => Some(
                lash_kernel_value::identity::SessionId::try_from(actor.id().to_owned())
                    .map_err(|_| corrupt("session id", actor.id()))?,
            ),
            lash_durable::ActorKind::Process => None,
        };
        // One statement: its own snapshot holds the actor, its mail and a
        // session's unfinished turn with the cancel it accepted.
        self.readers
            .next()
            .call(move |connection| {
                let mut statement = connection.prepare_cached(SQL.sqlite.open.sql())?;
                let mut rows = statement.query(rusqlite::params![
                    actor.as_str(),
                    session
                        .as_ref()
                        .map(lash_kernel_value::identity::SessionId::as_str)
                ])?;
                let mut opened: Option<(OpenedActor, Option<_>)> = None;
                while let Some(row) = rows.next()? {
                    let (opened, _) = match &mut opened {
                        Some(opened) => opened,
                        None => {
                            let current = Epoch(row.get(0)?);
                            let state: String = row.get(1)?;
                            if current != epoch || state != ActorState::Owned.as_str() {
                                return Ok(Err(DurableError::OwnershipLost(Fenced {
                                    actor: actor.clone(),
                                    held: epoch,
                                    current: Some(current),
                                })));
                            }
                            let turn = match row.get::<_, Option<String>>(9)? {
                                Some(_) => Some((
                                    turns::StoredTurn::read(row, 9)?,
                                    row.get::<_, Option<String>>(24)?
                                        .map(|_| turns::StoredCancel::read(row, 24))
                                        .transpose()?,
                                )),
                                None => None,
                            };
                            opened.insert((
                                OpenedActor {
                                    actor: actor.clone(),
                                    epoch,
                                    revision: StateRevision(row.get(2)?),
                                    acked: MailSeq(row.get(3)?),
                                    seen: MailSeq(row.get(4)?),
                                    mail: Vec::new(),
                                    at,
                                    turn_cancel: None,
                                },
                                turn,
                            ))
                        }
                    };
                    if let Some(seq) = row.get::<_, Option<i64>>(5)?
                        && seq <= opened.seen.0
                    {
                        opened.mail.push(Mail {
                            seq: MailSeq(seq),
                            kind: MailKind::new(row.get::<_, String>(6)?),
                            body: row.get(7)?,
                            appended_at: DurableInstant(row.get(8)?),
                        });
                    }
                }
                let Some((mut opened, turn)) = opened else {
                    return Ok(Err(DurableError::OwnershipLost(Fenced {
                        actor,
                        held: epoch,
                        current: None,
                    })));
                };
                // The turn is decoded and refused as the turn read does.
                if let (Some((turn, cancel)), Some(session)) = (turn, &session) {
                    let decoded = turn.decode(session, |run| {
                        Ok(cancel.map(|cancel| cancel.decode(session, run)).transpose())
                    })?;
                    opened.turn_cancel = match decoded {
                        Ok(row) => row.cancel,
                        Err(error) => return Ok(Err(error)),
                    };
                }
                let requests = match run_records::call_cancels(connection, &opened.actor)? {
                    Ok(requests) => requests,
                    Err(error) => return Ok(Err(error)),
                };
                Ok(Ok(ActorTx::opened(opened).with_call_cancels(requests)))
            })
            .await
            .map_err(store_failure)?
    }

    async fn begin_retained(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
        expected: StateRevision,
        max_mail_bytes: u64,
    ) -> Result<ActorTx, DurableError> {
        let actor = actor.clone();
        let at = self.instant()?;
        let capacity = i64::try_from(max_mail_bytes).unwrap_or(i64::MAX);
        self.readers
            .next()
            .call(move |connection| {
                let mut statement = connection.prepare_cached(SQL.sqlite.open_retained.sql())?;
                let mut rows = statement.query(rusqlite::params![
                    actor.as_str(),
                    (actor.kind() == lash_durable::ActorKind::Session).then(|| actor.id()),
                    capacity
                ])?;
                let mut opened: Option<OpenedActor> = None;
                let mut partial = false;
                while let Some(row) = rows.next()? {
                    if opened.is_none() {
                        let current = Epoch(row.get(0)?);
                        let state: String = row.get(1)?;
                        if current != epoch || state != ActorState::Owned.as_str() {
                            return Ok(Err(DurableError::OwnershipLost(Fenced {
                                actor,
                                held: epoch,
                                current: Some(current),
                            })));
                        }
                        if StateRevision(row.get(2)?) != expected {
                            return Ok(Err(DurableError::RevisionConflict {
                                actor: actor.clone(),
                                epoch,
                                expected,
                                actual: StateRevision(row.get(2)?),
                            }));
                        }
                        let turn_cancel = if let Some(run) = row.get::<_, Option<String>>(11)? {
                            let session = lash_kernel_value::identity::SessionId::try_from(
                                actor.id().to_owned(),
                            )
                            .map_err(|error| {
                                rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                            })?;
                            let run = lash_vocabulary::identity::TurnId::try_from(run).map_err(
                                |error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)),
                            )?;
                            match turns::StoredCancel::read(row, 12)?.decode(&session, &run) {
                                Ok(cancel) => Some(cancel),
                                Err(error) => return Ok(Err(error)),
                            }
                        } else {
                            None
                        };
                        opened = Some(OpenedActor {
                            actor: actor.clone(),
                            epoch,
                            revision: expected,
                            acked: MailSeq(row.get(3)?),
                            seen: MailSeq(row.get(4)?),
                            mail: Vec::new(),
                            at,
                            turn_cancel,
                        });
                    }
                    let Some(opened) = opened.as_mut() else {
                        continue;
                    };
                    if let Some(seq) = row.get::<_, Option<i64>>(5)? {
                        if let Some(body) = row.get::<_, Option<String>>(9)? {
                            opened.mail.push(Mail {
                                seq: MailSeq(seq),
                                kind: MailKind::new(row.get::<_, String>(8)?),
                                body,
                                appended_at: DurableInstant(row.get(10)?),
                            });
                        } else {
                            partial = true;
                            if opened.mail.is_empty() && max_mail_bytes > 0 {
                                return Ok(Err(DurableError::MailboxItemTooLarge {
                                    actor: actor.clone(),
                                    bytes: u64::try_from(row.get::<_, i64>(6)?).unwrap_or(u64::MAX),
                                    limit: max_mail_bytes,
                                }));
                            }
                        }
                    }
                }
                let Some(mut opened) = opened else {
                    return Ok(Err(DurableError::OwnershipLost(Fenced {
                        actor,
                        held: epoch,
                        current: None,
                    })));
                };
                if partial {
                    opened.seen = opened.mail.last().map_or(opened.acked, |mail| mail.seq);
                }
                let requests = match run_records::call_cancels(connection, &opened.actor)? {
                    Ok(requests) => requests,
                    Err(error) => return Ok(Err(error)),
                };
                Ok(Ok(ActorTx::opened(opened).with_call_cancels(requests)))
            })
            .await
            .map_err(store_failure)?
    }

    async fn begin_process_retained(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
        expected: StateRevision,
        max_mail_bytes: u64,
    ) -> Result<(ActorTx, lash_durable::domain::ProcessFrontier), DurableError> {
        self.process_frontier(actor, epoch, expected, max_mail_bytes)
            .await
    }

    async fn open_process(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
    ) -> Result<(ActorTx, lash_durable::domain::ProcessView), DurableError> {
        self.open_process_view(actor, epoch).await
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        if tx.ack().is_some_and(|through| through > tx.seen()) {
            return Err(DurableError::AckBeyondRead {
                actor: tx.actor().clone(),
            });
        }
        let prepared = owner::prepare(tx, false, self.conn.fleet());
        let queued = Arc::clone(&prepared);
        let blob_profile = self.blob_profile;
        let answer = lash_core_ids::operational_metrics::sql::with_owner(
            prepared.write.actor().as_str().to_owned(),
            prepared.write.epoch().0,
            prepared.write.revision().0,
            self.write(label, move |connection, now| {
                apply_owner(connection, &queued, now, blob_profile, None)
            }),
        )
        .await;
        drop(prepared);
        answer
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        self.write(label, move |connection, now| {
            apply_mail(connection, tx, now)
        })
        .await
    }

    async fn claim_facts(
        &self,
        node: Option<&Owner>,
        actor: &ActorKey,
    ) -> Result<lash_durable::ClaimFacts, DurableError> {
        self.read_claim_facts(node, actor).await
    }

    /// No commit is in flight to a reader: each holds the database's one
    /// write lock from its first write to its end.
    async fn await_owner_commit(&self, _actor: &ActorKey) -> Result<(), DurableError> {
        Ok(())
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        let actor = actor.clone();
        self.read(move |tx| {
            let row = tx
                .prepare_cached(SQL.actor.snapshot.sql())?
                .query_row([actor.as_str()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, i64>(10)?,
                        row.get::<_, Option<String>>(11)?,
                        row.get::<_, i64>(12)?,
                    ))
                })
                .optional()?;
            let Some((
                _kind,
                state,
                epoch,
                node,
                boot,
                seq,
                acked,
                due,
                revision,
                formats,
                mail,
                park,
                failed,
            )) = row
            else {
                return Ok(Ok(None));
            };
            let snapshot = (|| {
                Ok(ActorSnapshot {
                    state: actor_state(&state)?,
                    epoch: Epoch(epoch),
                    owner: owner(node, boot),
                    has_mail: seq > acked,
                    next_due: due.map(DurableInstant),
                    revision: StateRevision(revision),
                    formats: lash_durable::FormatSet::new(formats),
                    pending_mail: integer::<u64>(mail).map_err(store_failure)?,
                    park,
                    failed_activations: integer::<u32>(failed).map_err(store_failure)?,
                    actor: actor.clone(),
                })
            })();
            Ok(snapshot.map(Some))
        })
        .await
    }

    async fn actors_in(
        &self,
        formats: &lash_durable::FormatSet,
        after: Option<&ActorKey>,
        limit: usize,
    ) -> Result<Vec<ActorKey>, DurableError> {
        let formats = formats.as_str().to_owned();
        let after = after.map_or_else(String::new, |actor| actor.as_str().to_owned());
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.read(move |tx| {
            let keys = tx
                .prepare_cached(SQL.actor.in_formats.sql())?
                .query_map(rusqlite::params![formats, after, limit], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(keys.iter().map(|key| actor_key(key)).collect())
        })
        .await
    }
}

#[cfg(test)]
#[path = "../durable_tests.rs"]
mod tests;

#[cfg(test)]
mod actor_open_tests;

#[cfg(test)]
#[path = "../graph_store_tests.rs"]
mod graph_store_tests;

#[cfg(test)]
#[path = "../wait_law_tests.rs"]
mod wait_law_tests;

#[cfg(test)]
#[path = "../process_law_tests.rs"]
mod process_law_tests;

#[cfg(test)]
mod constraint_tests;
