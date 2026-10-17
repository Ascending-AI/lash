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

use std::sync::{Arc, LazyLock};

use lash_core_execution::Clock;
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
mod graph;
mod pages;
mod park;
pub(crate) mod processes;
mod prompts;

/// Drain one bounded page; the collection repeats pages in separate transactions.
///
/// A page releases settled prompt audits and, at most
/// [`MATERIAL_RELEASE_BATCH`](lash_store_sql::session::revisions::MATERIAL_RELEASE_BATCH)
/// each, the bodies of settled inputs and the payloads of applied commands
/// whose revision is no longer retained (FIG-6607).
pub(crate) fn release_material_batch(
    tx: &Connection,
    session: Option<&lash_sansio::SessionId>,
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
            rusqlite::params![session.map(lash_sansio::SessionId::as_str), limit],
        )?;
    }
    Ok(released)
}
#[path = "../durable_reads.rs"]
mod reads;
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

pub(crate) use node_wakes::TABLES as NODE_WAKES_TABLES;
pub(crate) use node_wakes::{NodeWakeWriter, SqliteNodeWakes};

// Connection-local buffers capture producers without a second dispatch list;
// each capture sets the writer's capture mark (`CaptureMark::register`).
pub(super) const NODE_WAKE_CAPTURE: &str = "
CREATE TEMP TABLE lash_woken (actor TEXT PRIMARY KEY);
CREATE TEMP TABLE lash_appended (actor TEXT PRIMARY KEY);
CREATE TEMP TRIGGER lash_capture_created AFTER INSERT ON main.actors
    WHEN NEW.state = 'ready' BEGIN
    INSERT OR IGNORE INTO lash_woken VALUES (NEW.actor_key);
    SELECT lash_wake_captured();
END;
CREATE TEMP TRIGGER lash_capture_woken AFTER UPDATE OF mail_seq ON main.actors
    WHEN NEW.mail_seq > OLD.mail_seq BEGIN
    INSERT OR IGNORE INTO lash_woken VALUES (NEW.actor_key);
    SELECT lash_wake_captured();
END;
CREATE TEMP TRIGGER lash_capture_appended AFTER INSERT ON main.process_events BEGIN
    INSERT OR IGNORE INTO lash_appended VALUES ('p/' || NEW.process_id);
    SELECT lash_wake_captured();
END;
";

pub(super) const NODE_WAKE_ROWS: &str = "
SELECT a.actor_key, a.state, a.owner_node, a.owner_boot
FROM temp.lash_woken w JOIN main.actors a ON a.actor_key = w.actor
UNION ALL SELECT actor, NULL, NULL, NULL FROM temp.lash_appended";
pub(super) const NODE_WAKE_PEERS: &str =
    "SELECT node_id FROM nodes WHERE draining = 0 AND heartbeat_expires_at_ms > ?1";
#[cfg(test)]
pub(super) const NODE_WAKE_ROLLBACK_MAIL: &str =
    "UPDATE actors SET mail_seq = mail_seq + 1 WHERE actor_key = ?1";

// The domain tables I0 creates in the durable core database (FIG-5194);
// each domain's own lane adds its tables beside them.
pub(crate) use graph::TABLES as GRAPH_TABLES;
pub(crate) use graph::{
    AUDIT_PAGE, GraphClosure, audit_page, graph_closure, graph_counts, install_staged, peel,
    retire_roots,
};
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
    session: &lash_sansio::SessionId,
) -> rusqlite::Result<Vec<String>> {
    use lash_durable::domain::{ExecKey, OwnerKey, ScopeKey};

    let mut roots = run_records::delete_under(tx, &OwnerKey::session_turns(session))?;
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
    process: &lash_sansio::ProcessId,
) -> rusqlite::Result<()> {
    let facts = run_records::delete_owner(
        tx,
        &lash_durable::domain::OwnerKey::Process(process.clone()).stored(),
    )?;
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
        "SELECT 'run_records ' || owner_key || ' ' || run_seq || '/' || ordinal || ' ' || record_ref
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
    requires TEXT,
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
        /// `?2` whose claim requirements are served by JSON array `?5`,
        /// including retired-owner tokens; process
        /// actors in another set or with another requirement with mail of
        /// kind `?4` (a cancel) pending; and session actors in a set of `?2`
        /// with another requirement whose open turn holds an `immediate`
        /// cancel request, with mail of kind `?6` (a close) pending, or
        /// whose close has not reached its tombstone (FIG-6072); oldest
        /// first, with the state that made each claimable, its format set
        /// and its requirements.
        claimable = "SELECT a.actor_key, a.state, a.formats, a.requires FROM actors a
             WHERE ((a.state = 'ready' AND a.ready_at_ms <= ?1)
                 OR (a.state = 'waiting' AND a.next_due_ms <= ?1))
               AND ((a.formats IN (SELECT value FROM json_each(?2))
                     AND (a.requires IS NULL OR NOT EXISTS (
                         SELECT 1 FROM json_each(a.requires) r
                         WHERE NOT EXISTS (
                             SELECT 1 FROM json_each(?5) s
                             WHERE s.value = r.value OR (
                                 substr(r.value, 1, 9) = 'callback:'
                                 AND substr(s.value, -1) = '/'
                                 AND substr(r.value, 1, length(s.value)) = s.value
                                 AND instr(substr(r.value, length(s.value) + 1), '/') = 0)))))
                 OR (a.kind = 'process' AND EXISTS (
                     SELECT 1 FROM actor_mail m
                     WHERE m.actor_key = a.actor_key AND m.seq > a.acked_seq
                       AND m.kind = ?4))
                 OR (a.kind = 'session' AND a.formats IN (SELECT value FROM json_each(?2))
                     AND (EXISTS (
                             SELECT 1 FROM turn_cancel_requests c
                             WHERE c.session_id = substr(a.actor_key, 3)
                               AND c.mode = 'immediate')
                         OR EXISTS (
                             SELECT 1 FROM actor_mail m
                             WHERE m.actor_key = a.actor_key AND m.seq > a.acked_seq
                               AND m.kind = ?6)
                         OR EXISTS (
                             SELECT 1 FROM session_close s
                             WHERE s.session_id = substr(a.actor_key, 3)
                               AND (s.done_step IS NULL OR s.done_step <> 'tombstone')))))
             ORDER BY COALESCE(a.ready_at_ms, a.next_due_ms), a.actor_key
             LIMIT ?3";

        /// Session `?1`'s unfinished turn with its phase row, in the turn
        /// read's column order, and the cancel request the turn accepted, in
        /// the cancel read's (FIG-6617): one statement, as the open's turn.
        turn = "SELECT r.run, r.admission_json, p.phase, p.phase_arg, p.iteration,
                    p.checkpoint_ref, p.model_request_ref, p.model_deadline_ms,
                    p.turn_deadline_ms, p.written_epoch, p.model_calls,
                    p.model_stream_from, p.working_execution_ref, p.answer_ref, p.tool_calls,
                    c.request_id, c.origin, c.reason, c.disposition, c.mode
             FROM session_runs AS r
             JOIN turn_phases AS p ON p.session_id = r.session_id AND p.run = r.run
             LEFT JOIN turn_cancel_requests AS c
                 ON c.session_id = r.session_id AND c.turn_id = r.run
             WHERE r.session_id = ?1 AND r.admission_json IS NOT NULL
               AND r.terminal_kind IS NULL";

        /// Actor `?1`'s epoch, state, revision and mail marks with each mail
        /// past its acknowledgement, in order (one row with NULL mail columns
        /// when none); and, when `?2` names its session, on every row that
        /// session's unfinished turn with its phase row, in the turn read's
        /// column order, and the cancel request the turn accepted, in the
        /// cancel read's (FIG-6399). One statement, so one snapshot answers
        /// the open; the turn is looked up once.
        open = "WITH t AS MATERIALIZED (
                 SELECT r.run, r.admission_json, p.phase, p.phase_arg, p.iteration,
                        p.checkpoint_ref, p.model_request_ref, p.model_deadline_ms,
                        p.turn_deadline_ms, p.written_epoch, p.model_calls,
                        p.model_stream_from, p.working_execution_ref, p.answer_ref, p.tool_calls,
                        c.request_id, c.origin, c.reason, c.disposition, c.mode
                 FROM session_runs AS r
                 JOIN turn_phases AS p ON p.session_id = r.session_id AND p.run = r.run
                 LEFT JOIN turn_cancel_requests AS c
                     ON c.session_id = r.session_id AND c.turn_id = r.run
                 WHERE r.session_id = ?2 AND r.admission_json IS NOT NULL
                   AND r.terminal_kind IS NULL)
             SELECT a.epoch, a.state, a.state_revision, a.acked_seq, a.mail_seq,
                    m.seq, m.kind, m.body, m.appended_at_ms, t.*
             FROM actors a
             LEFT JOIN actor_mail m ON m.actor_key = a.actor_key AND m.seq > a.acked_seq
             LEFT JOIN t ON TRUE
             WHERE a.actor_key = ?1
             ORDER BY m.seq";

        /// Give actor `?1` to boot `?3` of node `?2`, bumping its epoch. A
        /// claim that finds no commit advanced the actor since the previous
        /// claim counts one more failed activation; any such commit resets
        /// the count.
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
    clock: Arc<dyn Clock>,
    blob_profile: crate::BuiltinBlobProfile,
    observer: lash_core_execution::facade_support::StoreObserver,
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
        clock: Arc<dyn Clock>,
        blob_profile: crate::BuiltinBlobProfile,
        observer: lash_core_execution::facade_support::StoreObserver,
        graph_reads: lash_core_store::checkpoint_graph::GraphReadPolicy,
    ) -> Self {
        Self {
            conn,
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
        // Bytes a write's own read cannot decode never clear by themselves.
        StoreError::StoredDataCorrupt { .. } => StoreFailureKind::Corrupt,
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
    candidate: Option<&mut CandidatePublication>,
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
    // A session's unfinished turn's accepted cancel, read under the fence
    // before the commit writes: the receipt reports a cancel that arrived
    // after the owner's open.
    let (turn_cancel, open_roots) = match write.actor().kind() {
        lash_durable::ActorKind::Session => {
            let Ok(session) = lash_sansio::SessionId::try_from(write.actor().id().to_owned())
            else {
                return refuse(corrupt("session id", write.actor().id()));
            };
            match turns::turn_with_roots(tx, &session)? {
                Ok(Some((row, roots))) => (row.cancel, Some(roots)),
                Ok(None) => (None, None),
                Err(error) => return refuse(error),
            }
        }
        lash_durable::ActorKind::Process => (None, None),
    };
    let publications: Vec<_> = write
        .domain()
        .iter()
        .flat_map(DomainWrite::publications)
        .collect();
    if candidate.is_some()
        || !write.graph().is_empty()
        || !write.reads().is_empty()
        || !publications.is_empty()
    {
        let checked = match check_graph(write.graph(), write.reads(), &publications) {
            Ok(checked) => checked,
            Err(refusal) => return refuse(DurableError::from(refusal)),
        };
        // The budget admits the commit's own graph and writes; a
        // candidate's pages are bounded by its staging quota instead.
        if let Some(budget) = write.budget() {
            let admitted = measure_owner_commit(write.graph(), write.domain())
                .and_then(|measured| budget.admit(&measured));
            if let Err(refusal) = admitted {
                return refuse(DurableError::from(refusal));
            }
        }
        let installed = match candidate {
            None => graph::install(tx, &checked)?.map(drop),
            Some(candidate) => pages::install_candidate(tx, write.domain(), &checked, candidate)?,
        };
        if let Err(refusal) = installed {
            return refuse(DurableError::from(refusal));
        }
    }
    let committing = Committing {
        actor: write.actor(),
        epoch: write.epoch(),
        now,
        fleet,
        blob_profile,
        retired: Retired::default(),
        open_roots: turns::OpenRoots::read(open_roots),
    };
    for domain in write.domain() {
        if let Err(refusal) = apply_domain(tx, &committing, domain)? {
            return refuse(refusal);
        }
    }
    graph::hold_retired(tx, committing.retired, now)?;
    if let Some(formats) = write.formats() {
        cached_execute(
            tx,
            SQL.actor.stamp_formats.sql(),
            rusqlite::params![actor, formats.as_str()],
        )?;
    }
    if let Some(requires) = write.requires() {
        processes::stamp_requires(tx, &actor, requires)?;
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
            // A stop that reached the actor since the owner's read is not
            // lost to the park: the actor goes ready instead, its park kept,
            // and its claimer runs the stop (FIG-6099).
            let stopped = write.immediate_cancel_since_read(turn_cancel.as_ref())
                || tx
                    .prepare_cached(SQL.park.stop_mail_after.sql())?
                    .query_row(
                        rusqlite::params![
                            actor,
                            write.seen().0,
                            lash_durable::domain::CANCEL_MAIL,
                            lash_durable::domain::SESSION_CLOSE_MAIL
                        ],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
            let stored: String = if stopped {
                tx.prepare_cached(SQL.actor.release.sql())?.query_row(
                    rusqlite::params![actor, "ready", Option::<i64>::None, now.0],
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
            if rest == Release::Ready && !write.advances() {
                cached_execute(tx, SQL.actor.unadvanced.sql(), [&actor])?;
            }
            match actor_state(&stored) {
                Ok(state) => state,
                Err(error) => return refuse(error),
            }
        }
    };
    commit(ActorCommit {
        revision: StateRevision(revision),
        state,
        turn_cancel,
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

/// [`wake_session_within`] for a session store transaction at `at_ms`,
/// answering the actor it woke for the caller to hand to the store set's
/// wake sink once the transaction commits. An absent or deleted session,
/// and one whose actor already ended (its close finished), wakes nobody and
/// creates nothing; that is no refusal of the producer's write, which
/// refuses such a session itself.
pub(crate) fn wake_session_tx(
    tx: &Connection,
    session: &lash_sansio::SessionId,
    control: bool,
    at_ms: u64,
) -> Result<Option<Woken>, lash_core_execution::StoreError> {
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
        DomainWrite::RunFact(write) => run_records::apply(tx, committing, write),
        DomainWrite::ExecRoot(write) => snapshots::apply(tx, committing, write),
        DomainWrite::EffectAdmission(write) => graph::apply_admission(tx, committing, write),
        DomainWrite::ReleasePin(write) => graph::apply_release_pin(tx, committing, write),
        DomainWrite::Wait(write) => waits::apply(tx, committing, write),
        DomainWrite::Process(write) => processes::apply(tx, committing, write),
        DomainWrite::SessionClose(write) => session_close::apply(tx, committing, write),
        DomainWrite::ActorPark { reason_json } => {
            park::set(tx, committing.actor.as_str(), reason_json)
        }
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
                        expires_at.0
                    ],
                )?;
                commit(NodeLease {
                    owner: lease_owner,
                    decodes: spec.decodes,
                    serves: spec.serves.clone(),
                    ttl_millis: spec.ttl_millis,
                    expires_at,
                })
            })
            .await;
        #[cfg(feature = "perf-witness")]
        if answer.is_ok() {
            lash_core_execution::perf_witness::startup::record(
                lash_core_execution::perf_witness::startup::Phase::NodeRegistered,
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
        // One statement: its own snapshot holds the actor, its mail and a
        // session's unfinished turn with the cancel it accepted.
        self.conn
            .call(move |connection| {
                let mut statement = connection.prepare_cached(SQL.sqlite.open.sql())?;
                let mut rows = statement.query(rusqlite::params![
                    actor.as_str(),
                    session.as_ref().map(lash_sansio::SessionId::as_str)
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
                Ok(Ok(ActorTx::opened(opened)))
            })
            .await
            .map_err(store_failure)?
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        if tx.ack().is_some_and(|through| through > tx.seen()) {
            return Err(DurableError::AckBeyondRead {
                actor: tx.actor().clone(),
            });
        }
        let blob_profile = self.blob_profile;
        lash_core_execution::facade_support::sql::with_owner(
            tx.actor().as_str().to_owned(),
            tx.epoch().0,
            tx.revision().0,
            self.write(label, move |connection, now| {
                apply_owner(connection, tx, now, blob_profile, None)
            }),
        )
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
