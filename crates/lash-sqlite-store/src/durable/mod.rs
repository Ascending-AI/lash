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
//! first; SQLite serializes writers, so a claim needs no row lock. Instants
//! come from the store set's injected clock, read once per transaction.

use std::sync::{Arc, LazyLock};

use lash_core_execution::Clock;
use lash_durable::domain::{
    ExecKey, OwnerKey, ParkEventRow, ParkEventSeq, ProcessActorRow, RunRecordRow, ScopeKey,
    SnapshotRow, TurnRow, WaitId, WaitRow,
};
use lash_durable::{
    ActorCommit, ActorKey, ActorKind, ActorSnapshot, ActorState, ActorTx, BootId, ClaimCause,
    Claimed, CommitLabel, DomainWrite, DurableError, DurableInstant, DurableReads, DurableStore,
    Epoch, Fenced, HeartbeatOutcome, Mail, MailAnswer, MailCommit, MailDomainWrite, MailKind,
    MailRefusal, MailSeq, MailTx, MailWrite, NodeId, NodeLease, NodeSpec, OpenedActor, Owner,
    Reaped, Release, StateRevision, StoreFailure, StoreFailureKind, Woken,
};
use lash_store_sql::durable::{ActorStatements, MailStatements, NodeStatements};
use rusqlite::{Connection, OptionalExtension};

use crate::conn::{SqliteConnection, TxOutcome, cached_execute};

mod park_events;
mod processes;
mod run_records;
mod session_close;
mod snapshots;
mod turns;
mod waits;

// The domain tables I0 creates in the durable core database (FIG-5194);
// each domain's own lane adds its tables beside them.
pub(crate) use run_records::TABLES as RUN_RECORDS_TABLES;
pub(crate) use snapshots::TABLES as EXEC_SNAPSHOTS_TABLES;

/// The engine's tables, carried by the durable core.
pub(crate) const DURABLE_TABLES: &str = "
CREATE TABLE IF NOT EXISTS nodes (
    node_id TEXT PRIMARY KEY,
    boot_id TEXT NOT NULL,
    formats_json TEXT NOT NULL,
    registered_at_ms INTEGER NOT NULL,
    heartbeat_expires_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS actors (
    actor_key TEXT PRIMARY KEY,
    kind TEXT NOT NULL CONSTRAINT ck_actors_kind CHECK (kind IN ('session', 'process')),
    state TEXT NOT NULL CONSTRAINT ck_actors_state
        CHECK (state IN ('idle', 'ready', 'owned', 'waiting', 'terminal')),
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
    CONSTRAINT ck_actors_owned CHECK ((state = 'owned') = (owner_node IS NOT NULL)),
    CONSTRAINT ck_actors_owner_boot CHECK ((owner_node IS NULL) = (owner_boot IS NULL)),
    CONSTRAINT ck_actors_ready CHECK ((state = 'ready') = (ready_at_ms IS NOT NULL)),
    CONSTRAINT ck_actors_due CHECK (next_due_ms IS NULL OR state = 'waiting'),
    CONSTRAINT ck_actors_mail CHECK (acked_seq <= mail_seq)
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
        /// Whether boot `?2` of node `?1` holds a lease.
        node_live = "SELECT 1 FROM nodes WHERE node_id = ?1 AND boot_id = ?2";

        /// Up to `?3` actors claimable at `?1` in a format set of JSON array
        /// `?2`, oldest first, with the state that made each claimable.
        claimable = "SELECT actor_key, state FROM actors
             WHERE ((state = 'ready' AND ready_at_ms <= ?1)
                 OR (state = 'waiting' AND next_due_ms <= ?1))
               AND formats IN (SELECT value FROM json_each(?2))
             ORDER BY COALESCE(ready_at_ms, next_due_ms), actor_key
             LIMIT ?3";

        /// Give actor `?1` to boot `?3` of node `?2`, bumping its epoch.
        claim = "UPDATE actors
             SET state = 'owned', epoch = epoch + 1, owner_node = ?2, owner_boot = ?3,
                 ready_at_ms = NULL, next_due_ms = NULL
             WHERE actor_key = ?1
             RETURNING epoch";
    }
}

struct Sql {
    node: NodeStatements,
    actor: ActorStatements,
    mail: MailStatements,
    sqlite: SqliteDurableStatements,
}

static SQL: LazyLock<Sql> = LazyLock::new(|| {
    let dialect = crate::schema_layout::MAIN;
    Sql {
        node: NodeStatements::render(dialect),
        actor: ActorStatements::render(dialect),
        mail: MailStatements::render(dialect),
        sqlite: SqliteDurableStatements::render(dialect),
    }
});

/// The [`DurableStore`] over a SQLite store set's durable core.
#[derive(Clone)]
pub struct SqliteDurableStore {
    conn: SqliteConnection,
    clock: Arc<dyn Clock>,
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
#[expect(
    dead_code,
    reason = "the domain writes that read it are V0, L4, L5 and L6 stubs (I0, FIG-5194)"
)]
pub(crate) struct Committing<'a> {
    /// The actor whose fence matched.
    pub(crate) actor: &'a ActorKey,
    /// The epoch it matched at; domain rows record it as `written_epoch`.
    pub(crate) epoch: Epoch,
    /// The transaction's one clock reading.
    pub(crate) now: DurableInstant,
}

fn refuse<T>(error: DurableError) -> Flow<T> {
    Ok(TxOutcome::Rollback(Err(error)))
}

fn commit<T>(value: T) -> Flow<T> {
    Ok(TxOutcome::Commit(Ok(value)))
}

impl SqliteDurableStore {
    pub(crate) fn new(conn: SqliteConnection, clock: Arc<dyn Clock>) -> Self {
        Self { conn, clock }
    }

    fn instant(&self) -> DurableInstant {
        DurableInstant(i64::try_from(self.clock.timestamp_ms()).unwrap_or(i64::MAX))
    }

    async fn write<T, F>(&self, label: CommitLabel, body: F) -> Result<T, DurableError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection, DurableInstant) -> Flow<T> + Send + 'static,
    {
        let now = self.instant();
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
    tx.prepare_cached(SQL.sqlite.node_live.sql())?
        .query_row([node.node.as_str(), node.boot.as_str()], |_| Ok(()))
        .optional()
        .map(|row| row.is_some())
}

fn apply_owner(tx: &Connection, write: ActorTx, now: DurableInstant) -> Flow<ActorCommit> {
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
    };
    for domain in write.domain() {
        if let Err(refusal) = apply_domain(tx, &committing, domain)? {
            return refuse(refusal);
        }
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
/// redrive: L6 (FIG-5175) adds the `parked` state, which only a control wake
/// readies; until then every wake readies the same states.
pub(crate) fn wake_within(
    tx: &Connection,
    actor: &ActorKey,
    control: bool,
    now: DurableInstant,
) -> Answer<(Woken, MailSeq)> {
    // L6 (FIG-5175) adds `parked`, which only a control wake readies.
    let _ = control;
    let woke = tx
        .prepare_cached(SQL.actor.wake.sql())?
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

/// Apply one owner-commit domain write by its domain's module.
fn apply_domain(tx: &Connection, committing: &Committing<'_>, write: &DomainWrite) -> Answer<()> {
    match write {
        DomainWrite::Turn(write) => turns::apply(tx, committing, write),
        DomainWrite::SessionCommit(write) => turns::apply_session_commit(tx, committing, write),
        DomainWrite::RunRecord(write) => run_records::apply(tx, committing, write),
        DomainWrite::Snapshot(write) => snapshots::apply(tx, committing, write),
        DomainWrite::Wait(write) => waits::apply(tx, committing, write),
        DomainWrite::Process(write) => processes::apply(tx, committing, write),
        DomainWrite::SessionClose(write) => session_close::apply(tx, committing, write),
        DomainWrite::ParkEvent(write) => park_events::apply(tx, committing, write),
    }
}

/// Apply one mailbox domain write by its domain's module, with its answer
/// and the actor it woke.
fn apply_mail_domain(
    tx: &Connection,
    write: &MailDomainWrite,
    now: DurableInstant,
) -> Answer<(MailAnswer, Option<Woken>)> {
    Ok(match write {
        MailDomainWrite::ResolveWait(resolution) => waits::resolve(tx, resolution, now)?
            .map(|(answer, woken)| (MailAnswer::ResolveWait(answer), woken)),
        MailDomainWrite::RequestProcessCancel(request) => {
            processes::request_cancel(tx, request, now)?
                .map(|(answer, woken)| (MailAnswer::RequestProcessCancel(answer), woken))
        }
        MailDomainWrite::RequestTurnCancel(request) => turns::request_cancel(tx, request, now)?
            .map(|(answer, woken)| (MailAnswer::RequestTurnCancel(answer), woken)),
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

fn apply_mail(tx: &Connection, writes: MailTx, now: DurableInstant) -> Flow<MailCommit> {
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
                let (answer, woken) = match apply_mail_domain(tx, domain, now)? {
                    Ok(applied) => applied,
                    Err(error) => return refuse(error),
                };
                receipt.answers.push(answer);
                if let Some(woken) = woken {
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
        Ok(self.instant())
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
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.write(CommitLabel::CLAIM, move |tx, now| {
            if !node_live(tx, &owner)? {
                return refuse(DurableError::NodeLeaseLost {
                    node: owner.node.clone(),
                });
            }
            let candidates = tx
                .prepare_cached(SQL.sqlite.claimable.sql())?
                .query_map(rusqlite::params![now.0, formats, limit], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut claimed = Vec::with_capacity(candidates.len());
            for (key, state) in candidates {
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
                });
            }
            commit(claimed)
        })
        .await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        let owner = node.owner.clone();
        self.read(move |tx| {
            let rows = tx
                .prepare_cached(SQL.actor.owned_by.sql())?
                .query_map([owner.node.as_str(), owner.boot.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows
                .into_iter()
                .map(|(key, epoch)| {
                    actor_key(&key).map(|actor| Claimed {
                        actor,
                        epoch: Epoch(epoch),
                        cause: ClaimCause::Adopted,
                    })
                })
                .collect())
        })
        .await
    }

    async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
        let actor = actor.clone();
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
        self.write(label, move |connection, now| {
            apply_owner(connection, tx, now)
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
                    ))
                })
                .optional()?;
            let Some((kind, state, epoch, node, boot, seq, acked, due, revision, formats, mail)) =
                row
            else {
                return Ok(Ok(None));
            };
            let snapshot = (|| {
                Ok(ActorSnapshot {
                    kind: ActorKind::parse(&kind).ok_or_else(|| corrupt("actor kind", &kind))?,
                    state: actor_state(&state)?,
                    epoch: Epoch(epoch),
                    owner: owner(node, boot),
                    has_mail: seq > acked,
                    next_due: due.map(DurableInstant),
                    revision: StateRevision(revision),
                    formats: lash_durable::FormatSet::new(formats),
                    pending_mail: u64::try_from(mail).unwrap_or_default(),
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

    async fn park_events(
        &self,
        after: Option<ParkEventSeq>,
        limit: usize,
    ) -> Result<Vec<ParkEventRow>, DurableError> {
        self.read(move |tx| park_events::read(tx, after, limit))
            .await
    }
}

#[cfg(test)]
#[path = "../durable_tests.rs"]
mod tests;
