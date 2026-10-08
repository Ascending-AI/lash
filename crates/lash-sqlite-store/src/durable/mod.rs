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
//! published after commit, never inside a writing transaction, and each
//! boot's liveness lock, a file lock its listener holds
//! ([`crate::liveness_locks`]).

use std::sync::{Arc, LazyLock};

use lash_core_execution::Clock;
use lash_durable::domain::{
    ExecKey, OwnerKey, ParkEventRow, ParkEventSeq, ProcessActorRow, RunRecordRow, ScopeKey,
    SessionCloseRow, SnapshotRow, TurnRow, WaitId, WaitRow,
};
use lash_durable::{
    ActorCommit, ActorKey, ActorSnapshot, ActorState, ActorTx, BootId, ClaimCause, Claimed,
    CommitLabel, DomainWrite, DurableError, DurableInstant, DurableReads, DurableStore, Epoch,
    Fenced, HeartbeatOutcome, Mail, MailAnswer, MailCommit, MailDomainWrite, MailKind, MailRefusal,
    MailSeq, MailTx, MailWrite, NodeId, NodeLease, NodeSpec, OpenedActor, Owner, Reaped, Release,
    StateRevision, StoreFailure, StoreFailureKind, Woken,
};
use lash_store_sql::durable::park_events::ParkEventStatements;
use lash_store_sql::durable::processes::{ActorParkStatements, ProcessActorStatements};
use lash_store_sql::durable::session_mail::SessionMailStatements;
use lash_store_sql::durable::{ActorStatements, MailStatements, NodeStatements};
use rusqlite::{Connection, OptionalExtension};

use crate::conn::{FencedTx, SqliteConnection, TxOutcome, cached_execute};
use crate::liveness_locks::{LivenessLocks, Probed};

mod park_events;
pub(crate) mod processes;
mod prompts;
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

pub(crate) use node_wakes::SqliteNodeWakes;
pub(crate) use node_wakes::TABLES as NODE_WAKES_TABLES;

// The domain tables I0 creates in the durable core database (FIG-5194);
// each domain's own lane adds its tables beside them.
pub(crate) use park_events::TABLES as PARK_EVENTS_TABLES;
pub(crate) use prompts::TABLES as PROMPT_SNAPSHOTS_TABLES;
pub(crate) use prompts::release as release_prompt_snapshots;
pub(crate) use run_records::TABLES as RUN_RECORDS_TABLES;
pub(crate) use session_close::TABLES as SESSION_CLOSE_TABLES;
pub(crate) use snapshots::TABLES as EXEC_SNAPSHOTS_TABLES;
pub(crate) use turns::TABLES as TURN_PHASES_TABLES;
pub(crate) use waits::TABLES as WAITS_TABLES;

/// The engine's tables, carried by the durable core.
pub(crate) const DURABLE_TABLES: &str = "
CREATE TABLE IF NOT EXISTS nodes (
    node_id TEXT PRIMARY KEY,
    boot_id TEXT NOT NULL,
    formats_json TEXT NOT NULL,
    registered_at_ms INTEGER NOT NULL,
    heartbeat_expires_at_ms INTEGER NOT NULL,
    draining INTEGER NOT NULL DEFAULT 0 CONSTRAINT ck_nodes_draining CHECK (draining IN (0, 1))
);

CREATE TABLE IF NOT EXISTS actors (
    actor_key TEXT PRIMARY KEY,
    kind TEXT NOT NULL CONSTRAINT ck_actors_kind CHECK (kind IN ('session', 'process')),
    state TEXT NOT NULL CONSTRAINT ck_actors_state
        CHECK (state IN ('idle', 'ready', 'owned', 'waiting', 'parked', 'terminal')),
    epoch INTEGER NOT NULL,
    owner_node TEXT,
    owner_boot TEXT,
    ready_at_ms INTEGER,
    next_due_ms INTEGER,
    formats TEXT NOT NULL,
    state_revision INTEGER NOT NULL,
    mail_seq INTEGER NOT NULL,
    acked_seq INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    park_json TEXT,
    failed_activations INTEGER NOT NULL DEFAULT 0,
    claimed_revision INTEGER,
    CONSTRAINT ck_actors_parked CHECK (state <> 'parked' OR park_json IS NOT NULL),
    CONSTRAINT ck_actors_owned CHECK ((state = 'owned') = (owner_node IS NOT NULL)),
    CONSTRAINT ck_actors_owner_boot CHECK ((owner_node IS NULL) = (owner_boot IS NULL)),
    CONSTRAINT ck_actors_ready CHECK ((state = 'ready') = (ready_at_ms IS NOT NULL)),
    CONSTRAINT ck_actors_due CHECK (next_due_ms IS NULL OR state = 'waiting'),
    CONSTRAINT ck_actors_mail CHECK (acked_seq <= mail_seq),
    CONSTRAINT ck_actors_key_kind CHECK (
        (kind = 'session' AND substr(actor_key, 1, 2) = 's/') OR
        (kind = 'process' AND substr(actor_key, 1, 2) = 'p/'))
);
CREATE INDEX IF NOT EXISTS ix_actors_ready ON actors (ready_at_ms) WHERE state = 'ready';
CREATE INDEX IF NOT EXISTS ix_actors_due ON actors (next_due_ms)
    WHERE state = 'waiting' AND next_due_ms IS NOT NULL;
CREATE INDEX IF NOT EXISTS ix_actors_owner ON actors (owner_node, owner_boot)
    WHERE state = 'owned';

CREATE TABLE IF NOT EXISTS actor_mail (
    actor_key TEXT NOT NULL REFERENCES actors (actor_key),
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    body TEXT NOT NULL,
    appended_at_ms INTEGER NOT NULL,
    PRIMARY KEY (actor_key, seq)
);
";

lash_store_sql::statements! {
    /// The engine statements only SQLite issues.
    pub(crate) struct SqliteDurableStatements @ "durable_sqlite" {
        /// Whether boot `?2` of node `?1` holds a lease: its draining flag,
        /// or no row.
        node_live = "SELECT draining FROM nodes WHERE node_id = ?1 AND boot_id = ?2";

        /// Every registered boot.
        boots = "SELECT node_id, boot_id FROM nodes ORDER BY node_id, boot_id";

        /// Up to `?3` actors claimable at `?1` in a format set of JSON array
        /// `?2`, or process actors in another set with mail of kind `?4` (a
        /// cancel) pending, oldest first, with the state that made each
        /// claimable and its format set.
        claimable = "SELECT a.actor_key, a.state, a.formats FROM actors a
             WHERE ((a.state = 'ready' AND a.ready_at_ms <= ?1)
                 OR (a.state = 'waiting' AND a.next_due_ms <= ?1))
               AND (a.formats IN (SELECT value FROM json_each(?2))
                 OR (a.kind = 'process' AND EXISTS (
                     SELECT 1 FROM actor_mail m
                     WHERE m.actor_key = a.actor_key AND m.seq > a.acked_seq
                       AND m.kind = ?4)))
             ORDER BY COALESCE(a.ready_at_ms, a.next_due_ms), a.actor_key
             LIMIT ?3";

        /// Give actor `?1` to boot `?3` of node `?2`, bumping its epoch. A
        /// claim that finds no commit since the previous claim counts one
        /// more failed activation; any commit since resets the count.
        claim = "UPDATE actors
             SET state = 'owned', epoch = epoch + 1, owner_node = ?2, owner_boot = ?3,
                 ready_at_ms = NULL, next_due_ms = NULL,
                 failed_activations = CASE WHEN claimed_revision = state_revision
                                           THEN failed_activations + 1 ELSE 0 END,
                 claimed_revision = state_revision
             WHERE actor_key = ?1
             RETURNING epoch";
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
    sqlite: SqliteDurableStatements,
}

static SQL: LazyLock<Sql> = LazyLock::new(|| {
    let dialect = crate::schema_layout::MAIN;
    Sql {
        node: NodeStatements::render(dialect),
        actor: ActorStatements::render(dialect),
        mail: MailStatements::render(dialect),
        park: ActorParkStatements::render(dialect),
        process: ProcessActorStatements::render(dialect),
        park_events: ParkEventStatements::render(dialect),
        session_mail: SessionMailStatements::render(dialect),
        sqlite: SqliteDurableStatements::render(dialect),
    }
});

/// The [`DurableStore`] over a SQLite store set's durable core.
#[derive(Clone)]
pub struct SqliteDurableStore {
    conn: SqliteConnection,
    clock: Arc<dyn Clock>,
    blob_profile: crate::BuiltinBlobProfile,
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
    /// The actor whose fence matched.
    pub(crate) actor: &'a ActorKey,
    /// The epoch it matched at; domain rows record it as `written_epoch`.
    pub(crate) epoch: Epoch,
    /// The transaction's one clock reading.
    pub(crate) now: DurableInstant,
    /// The fleet format the transaction's fence read: registry rows a
    /// domain write touches are encoded under it.
    pub(crate) fleet: lash_core_execution::FleetFormat,
    /// How the session store a head commit writes keeps its blobs.
    pub(crate) blob_profile: crate::BuiltinBlobProfile,
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
        clock: Arc<dyn Clock>,
        blob_profile: crate::BuiltinBlobProfile,
    ) -> Self {
        Self {
            conn,
            clock,
            blob_profile,
        }
    }

    fn instant(&self) -> Result<DurableInstant, DurableError> {
        integer(self.clock.timestamp_ms())
            .map(DurableInstant)
            .map_err(store_failure)
    }

    async fn write<T, F>(&self, label: CommitLabel, body: F) -> Result<T, DurableError>
    where
        T: Send + 'static,
        F: FnOnce(&FencedTx<'_>, DurableInstant) -> Flow<T> + Send + 'static,
    {
        let now = self.instant()?;
        tracing::trace!(label = label.as_str(), "durable sqlite commit");
        self.conn
            .write_flow(move |tx| body(tx, now))
            .await
            .map_err(store_failure)?
    }

    async fn read<T, F>(&self, body: F) -> Result<T, DurableError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<Result<T, DurableError>> + Send + 'static,
    {
        self.conn
            .read(move |tx| body(tx))
            .await
            .map_err(store_failure)?
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
    use lash_core_execution::StoreError;
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
        _ => kind,
    };
    DurableError::Store(StoreFailure {
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

fn formats_json(spec: &[lash_durable::FormatSet]) -> String {
    let formats: Vec<&str> = spec.iter().map(lash_durable::FormatSet::as_str).collect();
    serde_json::Value::from(formats).to_string()
}

/// The format sets a node's stored `formats_json` names.
fn decoded_sets(stored: &str) -> Result<Vec<lash_durable::FormatSet>, DurableError> {
    let sets: Vec<String> =
        serde_json::from_str(stored).map_err(|_| corrupt("node format sets", stored))?;
    Ok(sets.into_iter().map(lash_durable::FormatSet::new).collect())
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

/// The refusal for an owner whose fence did not match: its epoch against
/// the actor's current one.
fn fenced(tx: &Connection, actor: &ActorKey, held: Epoch) -> rusqlite::Result<DurableError> {
    let current = tx
        .prepare_cached(SQL.actor.epoch_of.sql())?
        .query_row([actor.as_str()], |row| row.get::<_, i64>(0))
        .optional()?;
    Ok(DurableError::OwnershipLost(Fenced {
        actor: actor.clone(),
        held,
        current: current.map(Epoch),
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

fn apply_owner(
    tx: &FencedTx<'_>,
    write: ActorTx,
    now: DurableInstant,
    blob_profile: crate::BuiltinBlobProfile,
) -> Flow<ActorCommit> {
    let fleet = tx.fleet();
    let actor = write.actor().as_str().to_owned();
    let fence = tx
        .prepare_cached(SQL.actor.fence.sql())?
        .query_row(rusqlite::params![actor, write.epoch().0], |row| {
            row.get::<_, i64>(0)
        })
        .optional()?;
    let Some(revision) = fence else {
        return refuse(fenced(tx, write.actor(), write.epoch())?);
    };
    let committing = Committing {
        actor: write.actor(),
        epoch: write.epoch(),
        now,
        fleet,
        blob_profile,
    };
    for domain in write.domain() {
        if let Err(refusal) = apply_domain(tx, &committing, domain)? {
            return refuse(refusal);
        }
    }
    if let Some(formats) = write.formats() {
        cached_execute(
            tx,
            SQL.actor.stamp_formats.sql(),
            rusqlite::params![actor, formats.as_str()],
        )?;
    }
    if let Some(through) = write.ack() {
        cached_execute(tx, SQL.actor.ack.sql(), rusqlite::params![actor, through.0])?;
        cached_execute(
            tx,
            SQL.mail.delete_through.sql(),
            rusqlite::params![actor, through.0],
        )?;
    }
    let state = match write.release() {
        None => ActorState::Owned,
        Some(Release::Parked) => {
            // A cancel that arrived since the owner's read is not lost to the
            // park: the actor goes ready instead, its park kept, and its
            // claimer ends it engine-free.
            let cancel_pending = tx
                .prepare_cached(SQL.park.pending_mail_of_kind.sql())?
                .query_row(
                    rusqlite::params![actor, lash_durable::domain::CANCEL_MAIL],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            let stored: String = if cancel_pending {
                tx.prepare_cached(SQL.actor.release.sql())?.query_row(
                    rusqlite::params![actor, "idle", Option::<i64>::None, now.0],
                    |row| row.get(0),
                )?
            } else {
                tx.prepare_cached(SQL.park.park.sql())?
                    .query_row([&actor], |row| row.get(0))?
            };
            match actor_state(&stored) {
                Ok(state) => state,
                Err(error) => return refuse(error),
            }
        }
        Some(Release::Terminal) => {
            cached_execute(tx, SQL.mail.delete_all.sql(), [&actor])?;
            let stored: String = tx
                .prepare_cached(SQL.actor.end.sql())?
                .query_row([&actor], |row| row.get(0))?;
            match actor_state(&stored) {
                Ok(state) => state,
                Err(error) => return refuse(error),
            }
        }
        Some(rest) => {
            let (state, due) = match rest {
                Release::Waiting { next_due } => ("waiting", next_due.map(|due| due.0)),
                Release::Ready => ("ready", None),
                _ => ("idle", None),
            };
            let stored: String = tx
                .prepare_cached(SQL.actor.release.sql())?
                .query_row(rusqlite::params![actor, state, due, now.0], |row| {
                    row.get(0)
                })?;
            match actor_state(&stored) {
                Ok(state) => state,
                Err(error) => return refuse(error),
            }
        }
    };
    commit(ActorCommit {
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

/// Wake session `session`'s actor inside the caller's transaction, creating
/// it first when this is its first work: the one call every producer of
/// session work makes beside its row (L3s, FIG-5196). The caller has
/// already refused an absent or deleted session, so the actor it creates
/// belongs to a live one. `control` marks a cancel.
pub(crate) fn wake_session_within(
    tx: &Connection,
    session: &lash_sansio::SessionId,
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

/// [`wake_session_within`] for a session store transaction at `at_ms`. An
/// absent or deleted session, and one whose actor already ended (its close
/// finished), wakes nobody and creates nothing; that is no refusal of the
/// producer's write, which refuses such a session itself.
pub(crate) fn wake_session_tx(
    tx: &Connection,
    session: &lash_sansio::SessionId,
    control: bool,
    at_ms: u64,
) -> Result<(), lash_core_execution::StoreError> {
    let (meta, deleted): (i64, i64) = tx
        .prepare_cached(SQL.session_mail.standing.sql())
        .and_then(|mut statement| {
            statement.query_row([session.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))
        })
        .map_err(crate::sqlite_error)?;
    if meta == 0 || deleted > 0 {
        return Ok(());
    }
    let now = DurableInstant(integer::<i64>(at_ms).map_err(crate::sqlite_error)?);
    match wake_session_within(tx, session, control, now).map_err(crate::sqlite_error)? {
        Ok(_) | Err(DurableError::MailRefused(MailRefusal::ActorTerminal(_))) => Ok(()),
        Err(error) => Err(lash_core_execution::StoreError::Backend(format!(
            "session {session} was not woken: {error}"
        ))),
    }
}

/// Apply one owner-commit domain write by its domain's module.
fn apply_domain(tx: &FencedTx<'_>, committing: &Committing<'_>, write: &DomainWrite) -> Answer<()> {
    match write {
        DomainWrite::Turn(write) => turns::apply(tx, committing, write),
        DomainWrite::SessionCommit(write) => turns::apply_session_commit(tx, committing, write),
        DomainWrite::RunRecord(write) => run_records::apply(tx, committing, write),
        DomainWrite::Snapshot(write) => snapshots::apply(tx, committing, write),
        DomainWrite::Wait(write) => waits::apply(tx, committing, write),
        DomainWrite::Process(write) => processes::apply(tx, committing, write),
        DomainWrite::SessionClose(write) => session_close::apply(tx, committing, write),
        DomainWrite::ParkEvent(write) => park_events::apply(tx, committing, write),
        DomainWrite::SessionMail(write) => session_mail::apply(tx, committing, write),
        DomainWrite::Prompt(write) => prompts::apply(tx, committing, write),
    }
}

/// Apply one mailbox domain write by its domain's module, with its answer
/// and the actors it woke.
fn apply_mail_domain(
    tx: &Connection,
    write: &MailDomainWrite,
    now: DurableInstant,
    fleet: lash_core_execution::FleetFormat,
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
        MailDomainWrite::RequestTurnCancel(request) => turns::request_cancel(tx, request, now)?
            .map(|(answer, woken)| {
                (
                    MailAnswer::RequestTurnCancel(answer),
                    woken.into_iter().collect(),
                )
            }),
        MailDomainWrite::Redrive(request) => park_events::redrive(tx, request, now)?
            .map(|(answer, woken)| (MailAnswer::Redrive(answer), woken.into_iter().collect())),
        // Each started process's actor is created ready in the start's own
        // transaction (ADR 0132 §12).
        MailDomainWrite::StartTrigger(start) => {
            crate::triggers::start::start_within(tx, start, now, fleet)?.map(|answer| {
                let woken = answer
                    .processes
                    .iter()
                    .filter_map(|process| ActorKey::process(process.as_str()).ok())
                    .map(|actor| Woken {
                        actor,
                        state: ActorState::Ready,
                        owner: None,
                    })
                    .collect();
                (MailAnswer::StartTrigger(answer), woken)
            })
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
                let woken = match wake_within(tx, actor, false, now)? {
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
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.instant()
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        let lease_owner = Owner {
            node: spec.node.clone(),
            boot: BootId::new(uuid::Uuid::new_v4().to_string()),
        };
        let formats = formats_json(&spec.decodes);
        let spec = spec.clone();
        self.write(CommitLabel::NODE_REGISTER, move |tx, now| {
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
                    expires_at.0
                ],
            )?;
            commit(NodeLease {
                owner: lease_owner,
                decodes: spec.decodes,
                ttl_millis: spec.ttl_millis,
                expires_at,
            })
        })
        .await
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
                .query_map([now.0], |row| {
                    Ok(Owner {
                        node: NodeId::new(row.get::<_, String>(0)?),
                        boot: BootId::new(row.get::<_, String>(1)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
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

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        let owner = node.owner.clone();
        let formats = formats_json(&node.decodes);
        let limit = integer::<i64>(limit).map_err(store_failure)?;
        let decodes = node.decodes.clone();
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
            let candidates = tx
                .prepare_cached(SQL.sqlite.claimable.sql())?
                .query_map(
                    rusqlite::params![now.0, formats, limit, lash_durable::domain::CANCEL_MAIL],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut claimed = Vec::with_capacity(candidates.len());
            for (key, state, stored_formats) in candidates {
                let epoch = tx.prepare_cached(SQL.sqlite.claim.sql())?.query_row(
                    rusqlite::params![key, owner.node.as_str(), owner.boot.as_str()],
                    |row| row.get::<_, i64>(0),
                )?;
                let actor = match actor_key(&key) {
                    Ok(actor) => actor,
                    Err(error) => return refuse(error),
                };
                claimed.push(Claimed {
                    actor,
                    epoch: Epoch(epoch),
                    cause: if state == "waiting" {
                        ClaimCause::Due
                    } else {
                        ClaimCause::Ready
                    },
                    purpose: lash_durable::ClaimPurpose::of(&decodes, &stored_formats),
                });
            }
            commit(claimed)
        })
        .await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        let owner = node.owner.clone();
        let decodes = node.decodes.clone();
        self.read(move |tx| {
            let rows = tx
                .prepare_cached(SQL.actor.owned_by.sql())?
                .query_map([owner.node.as_str(), owner.boot.as_str()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows
                .into_iter()
                .map(|(key, epoch, stored_formats)| {
                    actor_key(&key).map(|actor| Claimed {
                        actor,
                        epoch: Epoch(epoch),
                        cause: ClaimCause::Adopted,
                        purpose: lash_durable::ClaimPurpose::of(&decodes, &stored_formats),
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

    async fn live_decodes(&self) -> Result<Vec<Vec<lash_durable::FormatSet>>, DurableError> {
        let now = self.instant()?;
        self.read(move |tx| {
            let rows = tx
                .prepare_cached(SQL.node.live_decodes.sql())?
                .query_map([now.0], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.iter().map(|stored| decoded_sets(stored)).collect())
        })
        .await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        let actor = actor.clone();
        let at = self.instant()?;
        let session = match actor.kind() {
            lash_durable::ActorKind::Session => Some(
                lash_sansio::SessionId::try_from(actor.id().to_owned())
                    .map_err(|_| corrupt("session id", actor.id()))?,
            ),
            lash_durable::ActorKind::Process => None,
        };
        self.read(move |tx| {
            let mut statement = tx.prepare_cached(SQL.actor.open.sql())?;
            let mut rows = statement.query([actor.as_str()])?;
            let mut opened: Option<OpenedActor> = None;
            while let Some(row) = rows.next()? {
                let opened = match &mut opened {
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
                        opened.insert(OpenedActor {
                            actor: actor.clone(),
                            epoch,
                            revision: StateRevision(row.get(2)?),
                            acked: MailSeq(row.get(3)?),
                            seen: MailSeq(row.get(4)?),
                            mail: Vec::new(),
                            at,
                            turn_cancel: None,
                        })
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
            drop(rows);
            drop(statement);
            // A session's unfinished turn's accepted cancel, under the same
            // read.
            if let (Some(opened), Some(session)) = (&mut opened, &session) {
                opened.turn_cancel = match turns::turn(tx, session)? {
                    Ok(row) => row.and_then(|row| row.cancel),
                    Err(error) => return Ok(Err(error)),
                };
            }
            Ok(match opened {
                Some(opened) => Ok(ActorTx::opened(opened)),
                None => Err(DurableError::OwnershipLost(Fenced {
                    actor,
                    held: epoch,
                    current: None,
                })),
            })
        })
        .await
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        if tx.ack().is_some_and(|through| through > tx.seen()) {
            return Err(DurableError::AckBeyondRead {
                actor: tx.actor().clone(),
            });
        }
        let blob_profile = self.blob_profile;
        self.write(label, move |connection, now| {
            apply_owner(connection, tx, now, blob_profile)
        })
        .await
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
}

#[async_trait::async_trait]
impl DurableReads for SqliteDurableStore {
    async fn turn(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Option<TurnRow>, DurableError> {
        let session = session.clone();
        self.read(move |tx| turns::turn(tx, &session)).await
    }

    async fn turn_end(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Option<lash_durable::domain::TurnEnd>, DurableError> {
        let (session, run) = (session.clone(), run.clone());
        self.read(move |tx| turns::turn_end(tx, &session, &run))
            .await
    }

    async fn turn_namespaces(
        &self,
        session: &lash_sansio::SessionId,
        run: &lash_sansio::TurnId,
    ) -> Result<Vec<lash_durable::domain::TurnNamespace>, DurableError> {
        let (session, run) = (session.clone(), run.clone());
        self.read(move |tx| turns::turn_namespaces(tx, &session, &run))
            .await
    }

    async fn run_records(&self, owner: &OwnerKey) -> Result<Vec<RunRecordRow>, DurableError> {
        let owner = owner.clone();
        self.read(move |tx| run_records::read(tx, &owner)).await
    }

    async fn snapshot(&self, exec: &ExecKey) -> Result<Option<SnapshotRow>, DurableError> {
        let exec = exec.clone();
        self.read(move |tx| snapshots::read(tx, &exec)).await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
        let owner = owner.clone();
        self.read(move |tx| waits::pending(tx, &owner)).await
    }

    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
        let id = *id;
        self.read(move |tx| waits::wait(tx, &id)).await
    }

    async fn process(
        &self,
        process: &lash_sansio::ProcessId,
    ) -> Result<Option<ProcessActorRow>, DurableError> {
        let process = process.clone();
        self.read(move |tx| processes::process(tx, &process)).await
    }

    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<lash_sansio::ProcessId>, DurableError> {
        let scope = scope.clone();
        self.read(move |tx| processes::live_until_descendants(tx, &scope, limit))
            .await
    }

    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&lash_sansio::ProcessId>,
        limit: usize,
    ) -> Result<Vec<lash_sansio::ProcessId>, DurableError> {
        let scope = scope.clone();
        let after = after.cloned();
        self.read(move |tx| processes::until_children(tx, &scope, after.as_ref(), limit))
            .await
    }

    async fn session_close(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError> {
        let session = session.clone();
        self.read(move |tx| session_close::read(tx, &session)).await
    }

    async fn ending_scopes(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<Vec<ScopeKey>, DurableError> {
        let session = session.clone();
        self.read(move |tx| session_close::ending_scopes(tx, &session))
            .await
    }

    async fn session_mailbox(
        &self,
        session: &lash_sansio::SessionId,
    ) -> Result<lash_durable::domain::SessionMailbox, DurableError> {
        let session = session.clone();
        self.read(move |tx| session_mail::read(tx, &session)).await
    }

    async fn park_events(
        &self,
        after: Option<ParkEventSeq>,
        limit: usize,
    ) -> Result<Vec<ParkEventRow>, DurableError> {
        self.read(move |tx| park_events::read(tx, after, limit))
            .await
    }

    async fn prompt_snapshot(
        &self,
        call: &lash_durable::domain::PromptCallKey,
    ) -> Result<Option<lash_durable::domain::PromptSnapshotRow>, DurableError> {
        let call = call.clone();
        self.read(move |tx| prompts::snapshot(tx, &call)).await
    }

    async fn prompt_texts(
        &self,
        hashes: &[String],
    ) -> Result<Vec<lash_durable::domain::PromptText>, DurableError> {
        let hashes = hashes.to_vec();
        self.read(move |tx| prompts::texts(tx, &hashes)).await
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
mod constraint_tests {
    use super::*;
    fn assert_check_rejects(connection: &Connection, statement: &str, constraint: &str) {
        let error = connection
            .execute_batch(statement)
            .expect_err("an illegal durable vocabulary must violate its schema CHECK");
        assert!(
            error.to_string().contains(constraint),
            "SQLite reported the wrong CHECK for {constraint}: {error}"
        );
    }

    #[test]
    fn waits_refuse_impossible_purpose_and_lifecycle_rows() {
        let conn = Connection::open_in_memory().expect("open wait fixture");
        conn.execute_batch(crate::durable::WAITS_TABLES)
            .expect("create schema");
        conn.execute_batch("INSERT INTO waits (wait_id, owner_actor, owner_scope, kind, host_resolvable, state, created_epoch) VALUES ('wait', 's/session', 's/session', 'signal', 0, 'pending', 1)").expect("valid pending signal");
        for (assignment, constraint) in [
            ("kind = 'timer'", "ck_waits_timer_deadline"),
            ("resolved_at_ms = 1", "ck_waits_settled_at"),
            ("state = 'revoked'", "ck_waits_settled_at"),
            (
                "state = 'resolved', resolution_digest = 'digest', resolved_at_ms = 1",
                "ck_waits_resolution_ref",
            ),
            ("resolution_ref = 'payload'", "ck_waits_resolution_ref"),
        ] {
            assert_check_rejects(&conn, &format!("UPDATE waits SET {assignment}"), constraint);
        }
        conn.execute_batch("UPDATE waits SET kind = 'timer', deadline_ms = 1, state = 'resolved', resolution_digest = 'timer', resolved_at_ms = 1").expect("resolved timer needs no payload");
        assert_check_rejects(
            &conn,
            "UPDATE waits SET resolution_ref = 'payload'",
            "ck_waits_resolution_ref",
        );
    }

    #[test]
    fn actors_refuse_kind_that_disagrees_with_key() {
        let conn = Connection::open_in_memory().expect("open actor fixture");
        conn.execute_batch(crate::durable::DURABLE_TABLES)
            .expect("create schema");
        for (key, kind) in [
            ("s/session", "process"),
            ("p/process", "session"),
            ("x/unknown", "session"),
        ] {
            assert_check_rejects(
                &conn,
                &format!(
                    "INSERT INTO actors (actor_key, kind, state, epoch, formats, state_revision, mail_seq, acked_seq, created_at_ms) VALUES ('{key}', '{kind}', 'idle', 0, '[]', 0, 0, 0, 0)"
                ),
                "ck_actors_key_kind",
            );
        }
    }
}
