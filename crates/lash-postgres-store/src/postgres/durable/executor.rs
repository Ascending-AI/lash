//! The one-call owner commit on PostgreSQL (FIG-5936).
//!
//! An owner commit whose every domain write is a step program
//! ([`super::steps`]) is one server call: `lash_owner_commit`, a PL/pgSQL
//! function generated from this build's statements by
//! [`super::generator`] and declared in `schema.sql`. It runs the commit's envelope (the writer fence
//! shared, the owner fence and its turn-cancel read, the database clock),
//! installs the commit's graph, key-share locking every staged node another
//! writer stored, runs each step's statement as static SQL dispatched by the
//! statement's name, holds the roots the steps retired for retirement steps
//! (FIG-5949, FIG-6191), then the
//! formats stamp, the acknowledgement and the release, in one autocommit
//! transaction. Every statement keeps a cached plan: none is text the
//! function executes.
//!
//! A session head commit rides the same call (FIG-5942): it is planned by
//! the store's shared planner over what the owner's open read of the head
//! ([`super::head_commit`]), every fact it was planned on is a guard among
//! its steps, and its checkpoint's bodies and graph are installed with the
//! commit's own.
//!
//! The function decides nothing a domain owns. A step that answers another
//! number of rows than it must, an envelope that does not admit the commit
//! or a graph the store does not accept raises, which rolls the call back,
//! and the commit runs again in the explicit-transaction form, whose
//! classifiers name the refusal. A commit with a write that is not a step
//! program takes that form from the start.
//!
//! A lost answer is reconciled by the call's token: the function records
//! its answer under the token as its last write, and the reconciler inserts
//! the same token. A call that committed keeps its row, and its answer is
//! read back; a call still running holds the token's key, and the insert
//! waits for its end; a call that has not reached its insert, or never ran,
//! meets the reconciler's row and rolls back. So the reconciler's verdict is
//! final: a commit is never applied twice.

use std::sync::LazyLock;

use lash_durable::domain::DomainWrite;
use lash_durable::{
    ActorCommit, ActorTx, CommitLabel, DurableError, Epoch, Release, StoreFailure, StoreFailureKind,
};
use lash_store_sql::Dialect;

use super::steps::{Entry, Step};
use super::{
    PostgresDurableStore, SQL, get, graph, head_commit, model_calls, park, processes, prompts,
    replay, retention, run_records, session_close, session_mail, snapshots, sqlx_failure, turns,
    waits,
};
use crate::connection_sql::connection_sql;
use crate::replayable::Attempt;
use crate::session_sql::session_sql;

lash_store_sql::statements! {
    /// The statements of the one-call owner commit's own mechanics.
    pub(crate) struct ExecutorStatements @ "durable_executor" {
        /// How many of the named blobs `?1` are not stored, or are among
        /// the roots `?2` and not stored as graph nodes.
        external_absent = "SELECT count(*) FROM unnest(?1::bytea[]) AS named(hash)
             LEFT JOIN blobs AS stored ON stored.hash = named.hash
             WHERE stored.hash IS NULL
                OR (named.hash = ANY(?2::bytea[]) AND stored.encoding <> 1)";

        /// How many staged nodes `?1` (raw digests) with bytes `?2` the
        /// install left out (not among `?3`) whose stored blob is not the
        /// same graph node.
        held_conflicts = "SELECT count(*)
             FROM unnest(?1::bytea[], ?2::bytea[]) AS staged(digest, content)
             LEFT JOIN blobs AS stored ON stored.hash = staged.digest
             WHERE NOT (staged.digest = ANY(?3::bytea[]))
               AND (stored.hash IS NULL OR stored.encoding <> 1
                    OR stored.content <> staged.content)";

        /// The bytes of the staged nodes `?1` (raw digests) with bytes `?2`
        /// the install inserted (among `?3`).
        installed_bytes = "SELECT COALESCE(sum(octet_length(staged.content)), 0)::bigint
             FROM unnest(?1::bytea[], ?2::bytea[]) AS staged(digest, content)
             WHERE staged.digest = ANY(?3::bytea[])";

        /// Replace base and effective requirements together only when actor
        /// `?1` has no live execution debt. Otherwise the explicit owner
        /// transaction must decode and merge its registrations and resolutions.
        stamp_unregistered_requires = "UPDATE actors SET base_requires = ?2, requires = ?2
             WHERE actor_key = ?1 AND NOT EXISTS (
                 SELECT 1 FROM package_executions WHERE actor_key = ?1 AND ended = 0
             )";

        /// Record the answer `?3` of call `?1` at `?2`: the call's last
        /// write.
        latch_record = "INSERT INTO owner_commit_latches (token, created_ms, answer)
             VALUES (?1, ?2, ?3)";

        /// Forget a few answers recorded before `?1`, skipping any another
        /// sweep holds: no commit waits on another's sweep.
        latch_sweep = "DELETE FROM owner_commit_latches WHERE token IN (
                 SELECT token FROM owner_commit_latches WHERE created_ms < ?1
                 ORDER BY created_ms
                 LIMIT 16
                 FOR UPDATE SKIP LOCKED
             )";

        /// Take call `?1`'s token unless the call recorded it: a row when
        /// the call never commits.
        latch_preempt = "INSERT INTO owner_commit_latches (token, created_ms, answer)
             VALUES (?1, (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT, NULL)
             ON CONFLICT (token) DO NOTHING
             RETURNING token";

        /// The answer call `?1` recorded.
        latch_answer = "SELECT answer FROM owner_commit_latches WHERE token = ?1";
    }
}

pub(crate) static EXECUTOR_SQL: LazyLock<ExecutorStatements> =
    LazyLock::new(|| ExecutorStatements::render(Dialect::postgres()));

/// How long a recorded answer is kept: far past the reconciliation of a
/// lost call, which reads it within its operation's retries.
#[cfg(test)]
pub(super) const LATCH_HORIZON_MS: i64 = 900_000;

/// How many prompt texts a storage remembers it holds.
const HELD_TEXTS: usize = 4096;

/// The prompt texts a storage's commits recorded, most recent last, which
/// a one-call commit sends by address alone. A hint, never a fact: a text
/// reclaimed since refuses the call, and the commit runs in the explicit
/// form, which reads what the store holds.
#[derive(Debug, Default)]
pub(crate) struct HeldTexts {
    known: std::sync::Mutex<(
        std::collections::HashSet<String>,
        std::collections::VecDeque<String>,
    )>,
}

impl HeldTexts {
    pub(super) fn contains(&self, hash: &str) -> bool {
        self.known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
            .contains(hash)
    }

    /// Remember the texts `tx` recorded, once it committed.
    pub(super) fn note(&self, tx: &ActorTx) {
        let mut known = self
            .known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (set, order) = &mut *known;
        for write in tx.domain() {
            let DomainWrite::Prompt(lash_durable::domain::PromptWrite::Record {
                snapshot_texts,
                request_texts,
                prefix,
                ..
            }) = write
            else {
                continue;
            };
            // A held immutable root owns its closure. Remember publication
            // boundaries, not every descendant of a rebuilt history.
            for hash in snapshot_texts
                .iter()
                .map(|text| &text.hash)
                .chain(request_texts.roots())
                .chain(prefix)
            {
                if set.insert(hash.clone()) {
                    order.push_back(hash.clone());
                }
            }
        }
        while order.len() > HELD_TEXTS {
            if let Some(oldest) = order.pop_front() {
                set.remove(&oldest);
            }
        }
    }
}

/// The function's name.
pub(crate) const FUNCTION: &str = "lash_owner_commit";

/// Every statement a step may name: what the function dispatches on.
pub(super) fn entries() -> Vec<&'static Entry> {
    vec![
        &model_calls::APPEND,
        &model_calls::ROOT_MATCHES,
        &model_calls::RETIRE_PIN,
        &model_calls::RESERVE,
        &model_calls::ANSWERED,
        &run_records::APPEND,
        &run_records::PRUNE,
        &snapshots::PUT_FIRST,
        &snapshots::PUT_NEXT,
        &snapshots::RETIRE,
        &snapshots::CLEAR,
        &turns::ADMIT_RUN,
        &turns::INSERT_PHASE,
        &turns::ADVANCE_PHASE,
        &turns::ANSWER_SET,
        &turns::ADD_TOOL_CALLS,
        &turns::OPEN_NAMED_RUN,
        &turns::UPSERT_NAMESPACES,
        &turns::WORKING_SET,
        &turns::PHASE_ROOTS,
        &turns::NAMESPACE_ROOTS,
        &session_mail::BIND_INPUTS,
        &session_mail::BIND_BATCHES,
        &prompts::RECORD,
        &graph::ADMISSION_PUT,
        &graph::ADMISSION_REMOVE,
        &run_records::DELETE_OWNER,
        &graph::CLEAR_ADMISSIONS,
        &park::SET_PARK,
        // A turn's terminal (FIG-5942).
        &turns::LOCK_KEY,
        &turns::END_RUN,
        &turns::MARK_TERMINAL,
        &turns::RETENTION,
        &turns::SETTLE_HELD_INPUTS,
        &turns::SETTLE_HELD_BATCHES,
        &turns::END_LOCAL,
        &snapshots::CLEAR_UNDER,
        &graph::ADMISSIONS_UNDER,
        &run_records::DELETE_UNDER,
        &turns::TERMINAL_STATE,
        // What ends with a turn: its scope, its waits, its prompt texts.
        &waits::REVOKE_SCOPE,
        &waits::COMPACT_SCOPE,
        &waits::RESOLVE_PROCESS_TERMINAL,
        &waits::RESOLVE_CHILD_SESSION,
        &processes::LOCK_TURN,
        &processes::TURN_CLOSE,
        &processes::RELEASE_HOLDS,
        &processes::FORGET_HOLDS,
        &session_close::SCOPE_ENDING,
        &prompts::SETTLE,
        &prompts::EXPIRE_AUDITS,
        &prompts::HOLD_PREFIX,
        &prompts::RELEASE_PREFIX,
        &prompts::RECLAIM,
        // A session head commit.
        &head_commit::PLUGIN_WRITERS,
        &head_commit::LOCK_SESSION,
        &head_commit::DELETED,
        &head_commit::MATERIALIZED,
        &head_commit::RECEIPT,
        &head_commit::HEAD_AT,
        &head_commit::HEAD_ABSENT,
        &head_commit::LEAF,
        &head_commit::OCCUPIED,
        &head_commit::NODES,
        &head_commit::REVISION,
        &head_commit::HEAD_CAS,
        &head_commit::TOUCH,
        &head_commit::PRUNE,
        &retention::RELEASE_INPUT_BODIES,
        &retention::RELEASE_COMMAND_PAYLOADS,
        &retention::RELEASE_TOOL_INTENT_PAYLOADS,
        &head_commit::SETTLE_COMMAND,
        &head_commit::SETTLE_INPUT,
        &head_commit::RELEASE_INPUT,
        &head_commit::SETTLE_BATCH,
        &head_commit::RELEASE_BATCH,
        &head_commit::REFERRER_FENCED,
        &head_commit::LOCK_ATTACHMENT,
        &head_commit::ATTACHMENT_DELETING,
        &head_commit::ATTACHMENT_UPLOADED,
        &head_commit::ATTACHMENT_CLAIMED,
        &head_commit::ATTACHMENT_EDGE,
        &retention::HOLD_ATTACHMENT,
        &retention::QUEUE_RELEASED_INGRESS,
        &retention::END_RELEASED_INGRESS,
        &retention::RELEASE_ATTACHMENT_EDGES,
        &retention::DEQUEUE_ATTACHMENT_RELEASES,
        &head_commit::WORK_REMAINING,
        &head_commit::RECORD_RECEIPT,
        &head_commit::FENCE_REFERRER,
        &head_commit::CLEANUP_INSERT,
        &head_commit::CLEANUP_REPLACE,
        &head_commit::CARRY_EDGE,
    ]
}

/// `write`'s step program at `epoch`: `None` when it has none, and its
/// commit takes the explicit-transaction form. A session head commit's is
/// planned by [`Call::of`], over its owner's hint.
pub(super) fn program(
    actor: &lash_durable::ActorKey,
    epoch: Epoch,
    write: &DomainWrite,
    held: &HeldTexts,
) -> Result<Option<Vec<Step>>, DurableError> {
    match write {
        DomainWrite::Environment(_)
        | DomainWrite::RegisterExecution(_)
        | DomainWrite::EndExecution(_)
        | DomainWrite::AdoptExecution(_)
        | DomainWrite::SeedResolution(_) => Ok(None),
        DomainWrite::Turn(write) => turns::steps(epoch, write),
        DomainWrite::RunFact(write) => run_records::steps(epoch, write),
        DomainWrite::ExecRoot(write) => snapshots::steps(epoch, write),
        DomainWrite::EffectAdmission(write) => graph::admission_steps(epoch, write).map(Some),
        DomainWrite::SessionMail(write) => Ok(session_mail::steps(write)),
        DomainWrite::Prompt(write) => Ok(prompts::steps(epoch, write, held)),
        DomainWrite::ActorPark { reason_json } => {
            Ok(Some(vec![park::step(actor.as_str(), reason_json)]))
        }
        DomainWrite::Wait(write) => Ok(waits::steps(write)),
        DomainWrite::Process(write) => Ok(processes::steps(write, actor, epoch)),
        DomainWrite::SessionCommit(_) | DomainWrite::SessionClose(_) => Ok(None),
        DomainWrite::ModelCall(write) => Ok(Some(model_calls::steps(epoch, write))),
        DomainWrite::ModelCallPin(write) => Ok(Some(model_calls::pin_steps(write))),
    }
}

/// The digest of every statement the function runs, which a call names:
/// a function generated from other statements refuses it.
pub(crate) static REGISTRY: LazyLock<String> = LazyLock::new(|| {
    // FNV-1a over each name and text: a digest, not a defense.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |text: &str| {
        for byte in text.bytes().chain([0]) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed("owner-envelope-session-head-receipts-v1");
    for text in mechanics().into_iter().chain(prompts::record_mechanics()) {
        feed(text);
    }
    for entry in entries() {
        feed(entry.name());
        feed(entry.statement().sql());
    }
    format!("{hash:016x}")
});

/// The mechanics' statement texts, in the order the function runs them.
fn mechanics() -> Vec<&'static str> {
    let executor = &*EXECUTOR_SQL;
    let mut mechanics = vec![
        connection_sql().lock_xact_fleet_fence_shared.sql(),
        SQL.postgres.owner_envelope.sql(),
        crate::blobs::blob_sql().postgres.insert_chunk.sql(),
        graph::lock_nodes().sql(),
        executor.external_absent.sql(),
        graph::insert_nodes().sql(),
        graph::stored_nodes().sql(),
        executor.held_conflicts.sql(),
        graph::insert_edges().sql(),
        executor.installed_bytes.sql(),
        session_sql().graph_postgres.select_parent_for_update.sql(),
        session_sql().graph_postgres.exists_reachable.sql(),
        session_sql().graph_postgres.retire.sql(),
        SQL.actor.wake.sql(),
        // Prompt retirement mechanics follow the owner-domain statements.
        super::pages::hold_statements().0.sql(),
        super::pages::hold_statements().1.sql(),
        SQL.actor.stamp_formats.sql(),
        executor.stamp_unregistered_requires.sql(),
        SQL.postgres.ack_through.sql(),
        SQL.mail.delete_all.sql(),
        SQL.actor.end.sql(),
        SQL.actor.release.sql(),
        executor.latch_record.sql(),
        executor.latch_sweep.sql(),
    ];
    mechanics.extend(prompts::retirement_mechanics());
    mechanics
}

/// The call's arguments for `tx`, unless the commit takes the
/// explicit-transaction form: a write that is not a step program, or a
/// graph or budget the explicit form refuses.
pub(super) struct Call {
    pub(super) steps: serde_json::Value,
    /// The checkpoint bodies a head commit supplies, by content address.
    pub(super) body_hashes: Vec<crate::keys::Key>,
    pub(super) bodies: Vec<Vec<u8>>,
    pub(super) named: Vec<crate::keys::Key>,
    pub(super) roots: Vec<crate::keys::Key>,
    pub(super) digests: Vec<Vec<u8>>,
    pub(super) contents: Vec<Vec<u8>>,
    pub(super) parents: Vec<Vec<u8>>,
    pub(super) children: Vec<Vec<u8>>,
    pub(super) release: Option<&'static str>,
    pub(super) due: Option<i64>,
    /// The fleet format a head commit was planned under: the call is
    /// refused under any other.
    pub(super) planned_fleet: Option<lash_core_store::store::FleetFormat>,
    /// The hint the call's head commit planned on, and what the head is
    /// once the call commits.
    pub(super) hint: Option<((String, i64), Option<head_commit::SessionFacts>)>,
}

/// `step` as the function reads it.
fn step_json(step: &Step) -> serde_json::Value {
    let mut json = serde_json::json!({
        "s": step.entry.name(),
        "p": step.values.iter().map(super::steps::Value::json).collect::<Vec<_>>(),
    });
    if let Some(expect) = step.expect {
        json["n"] = expect.into();
    }
    if !step.retired.is_empty() {
        json["r"] = step.retired.clone().into();
    }
    if !step.ancestry.is_empty() {
        json["a"] = step.ancestry.clone().into();
    }
    if let Some(slot) = step.capture {
        json["c"] = slot.to_string().into();
    }
    if let Some(when) = &step.when {
        json["w"] = serde_json::json!([when.slot.to_string(), when.column, when.holds]);
    }
    json
}

impl Call {
    pub(super) fn of(
        tx: &ActorTx,
        held: &HeldTexts,
        hints: &head_commit::SessionHints,
        fleet: lash_core_store::store::FleetFormat,
    ) -> Option<Self> {
        if tx
            .domain()
            .iter()
            .any(|w| matches!(w, DomainWrite::Environment(_)))
            || tx.graph().values().any(|node| {
                node.envelope().is_ok_and(|e| {
                    e.kind == lash_core_store::checkpoint_graph::NodeKind::ENVIRONMENT_VERSION
                })
            })
        {
            return None;
        }
        // Root-lease work uses the explicit transaction, which returns the
        // acquired leases and applies releases under the owner fence.
        if tx.process_preimage().is_some()
            || !tx.root_leases().is_empty()
            || !tx.released_root_leases().is_empty()
        {
            return None;
        }
        let mut call = Self {
            steps: serde_json::Value::Null,
            body_hashes: Vec::new(),
            bodies: Vec::new(),
            named: Vec::new(),
            roots: Vec::new(),
            digests: Vec::new(),
            contents: Vec::new(),
            parents: Vec::new(),
            children: Vec::new(),
            release: None,
            due: None,
            planned_fleet: None,
            hint: None,
        };
        // The executions the commit ends are locked first, as publication
        // locks them: referrer, then artifact.
        let mut steps: Vec<serde_json::Value> = turns::lock_ending_executions(tx.domain())
            .ok()?
            .iter()
            .map(step_json)
            .collect();
        // A head commit's checkpoint graph is installed with the commit's.
        let mut checkpoint_nodes = std::collections::BTreeMap::new();
        for write in tx.domain() {
            let program = match write {
                DomainWrite::SessionCommit(write) => {
                    // One head commit a call, by the session's own actor,
                    // over what its open read.
                    if call.hint.is_some()
                        || turns::head_writer(tx.actor(), &write.session)
                            != lash_core_store::store::HeadWriter::SessionActor
                    {
                        return None;
                    }
                    let key = (tx.actor().as_str().to_owned(), tx.epoch().0);
                    let facts = hints.get(&key)?;
                    let planner = turns::prepared(write, fleet)?;
                    let program =
                        head_commit::program(&planner, head_commit::Basis::Hinted(&facts), 0)
                            .ok()??;
                    for (hash, body) in &program.checkpoint.supplied {
                        call.body_hashes.push(crate::keys::key(hash));
                        call.bodies.push(body.to_vec());
                    }
                    checkpoint_nodes = program.checkpoint.nodes;
                    call.planned_fleet = Some(fleet);
                    call.hint = Some((
                        key,
                        program.after.map(|head| head_commit::SessionFacts {
                            head,
                            plugin_writers: facts.plugin_writers,
                        }),
                    ));
                    program.steps
                }
                write => program(tx.actor(), tx.epoch(), write, held).ok()??,
            };
            steps.extend(program.iter().map(step_json));
        }
        call.steps =
            serde_json::json!({"version": 1, "expected_revision": tx.revision().0, "steps": steps});
        let publications: Vec<_> = tx
            .domain()
            .iter()
            .flat_map(DomainWrite::publications)
            .collect();
        if !tx.graph().is_empty()
            || !tx.reads().is_empty()
            || !publications.is_empty()
            || !checkpoint_nodes.is_empty()
        {
            if let Some(budget) = tx.budget() {
                lash_durable::domain::graph_commit::measure_owner_commit(tx.graph(), tx.domain())
                    .and_then(|measured| budget.admit(&measured))
                    .ok()?;
            }
            let staged;
            let graph = if checkpoint_nodes.is_empty() {
                tx.graph()
            } else {
                checkpoint_nodes.extend(tx.graph().iter().map(|(id, node)| (*id, node.clone())));
                staged = checkpoint_nodes;
                &staged
            };
            let checked =
                lash_durable::domain::graph_commit::check_graph(graph, tx.reads(), &publications)
                    .ok()?;
            let named: std::collections::BTreeSet<_> = checked
                .external
                .iter()
                .chain(checked.external_roots.iter())
                .collect();
            call.named = named
                .iter()
                .map(|node| crate::keys::key(node.to_hex()))
                .collect();
            call.roots = checked
                .external_roots
                .iter()
                .map(|node| crate::keys::key(node.to_hex()))
                .collect();
            for node in &checked.nodes {
                for child in &node.children {
                    call.parents.push(node.id.0.to_vec());
                    call.children.push(child.0.to_vec());
                }
                call.digests.push(node.id.0.to_vec());
                call.contents.push(node.bytes.to_vec());
            }
            // An exact reference is required present by the named-node
            // check, then byte-compared by the existing supplied-body check.
            // It cannot be recreated, and publishes no new edges.
            for node in tx.exact_reads().values() {
                let key = crate::keys::key(node.id.to_hex());
                if !call.named.contains(&key) {
                    call.named.push(key);
                }
                if !checked.nodes.iter().any(|staged| staged.id == node.id) {
                    call.digests.push(node.id.0.to_vec());
                    call.contents.push(node.bytes.to_vec());
                }
            }
            call.named.sort_by(|left, right| left.0.cmp(&right.0));
            call.named.dedup();
        }
        (call.release, call.due) = match tx.release() {
            None => (None, None),
            // A park reads what reached the actor since its owner's read,
            // which the explicit-transaction form compares (FIG-6099).
            Some(Release::Parked) => return None,
            Some(Release::Terminal) => (Some("terminal"), None),
            Some(Release::Waiting { next_due }) => (Some("waiting"), next_due.map(|due| due.0)),
            // A ready release that writes nothing else keeps the claim's
            // mark that nothing advanced the actor.
            Some(Release::Ready) if !tx.advances() => (Some("ready_unadvanced"), None),
            Some(Release::Ready) => (Some("ready"), None),
            Some(_) => (Some("idle"), None),
        };
        Some(call)
    }
}

/// The call's SQL.
pub(super) static CALL: LazyLock<String> = LazyLock::new(|| {
    let params: Vec<String> = (1..=24).map(|index| format!("${index}")).collect();
    format!("SELECT * FROM {FUNCTION}({})", params.join(", "))
});

/// Whether the function refused the call: a refusal the
/// explicit-transaction form names, a call built from other statements,
/// or a store without the function. The call rolled back.
fn declined(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database)
            if database.code().is_some_and(|code| code.starts_with("LX") || code == "42883")
    )
}

/// Whether the server answered the call itself, so it rolled back: any
/// error but a broken connection, the session ending under it (`57P`), or a
/// server fault (`58`, `XX`).
fn answered(error: &sqlx::Error) -> bool {
    let sqlx::Error::Database(database) = error else {
        return false;
    };
    database.code().is_some_and(|code| {
        !(code.starts_with("08")
            || code.starts_with("57P")
            || code.starts_with("58")
            || code.starts_with("XX"))
    })
}

impl PostgresDurableStore {
    /// Commit `tx` as one call: its receipt, or `None` when the function
    /// refused it and the explicit-transaction form must run it. A lost
    /// answer is reconciled by the call's token.
    pub(super) async fn call_owner(
        &self,
        label: CommitLabel,
        tx: &ActorTx,
        call: &Call,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Option<ActorCommit>, Attempt<DurableError>> {
        let route = self.route(label.capacity());
        let mut connection = crate::observed_sql::checkout(route.pool)
            .await
            .map_err(|error| replay::attempt(sqlx_failure(error)))?;
        crate::observed_sql::transaction_started();
        let token = uuid::Uuid::new_v4().to_string();
        let injected = self
            .injected_instant()
            .transpose()
            .map_err(Attempt::Failed)?;
        let writable = self.fence.writable();
        let bound = |version: u32| i32::try_from(version).unwrap_or(i32::MAX);
        // A head commit was planned under one fleet format: the call is
        // refused under any other, and the explicit form plans again.
        let (fleet_low, fleet_high) = match call.planned_fleet {
            Some(planned)
                if planned.version() < writable.min() || planned.version() > writable.max() =>
            {
                return Ok(None);
            }
            Some(planned) => (bound(planned.version()), bound(planned.version())),
            None => (bound(writable.min()), bound(writable.max())),
        };
        let session = match tx.actor().kind() {
            lash_durable::ActorKind::Session => Some(tx.actor().id()),
            lash_durable::ActorKind::Process => None,
        };
        let (lock_timeout, statement_ms) = route.prelude.call_guards();
        let query = crate::observed_sql::query(&CALL)
            .bind(REGISTRY.as_str())
            .bind(tx.actor().as_str())
            .bind(tx.epoch().0)
            .bind(session)
            .bind(fleet_low)
            .bind(fleet_high)
            .bind(&token)
            .bind(lock_timeout)
            .bind(statement_ms)
            .bind(&call.body_hashes)
            .bind(&call.bodies)
            .bind(&call.named)
            .bind(&call.roots)
            .bind(&call.digests)
            .bind(&call.contents)
            .bind(&call.parents)
            .bind(&call.children)
            .bind(&call.steps)
            .bind(tx.formats().map(|formats| formats.as_str().to_owned()))
            .bind(tx.ack().map(|through| through.0))
            .bind(call.release)
            .bind(call.due)
            .bind(injected.map(|now| now.0))
            .bind(tx.requires().map(lash_durable::encode_requires));
        #[cfg(any(test, feature = "testing"))]
        let loses_answer = match self.commit_fault.as_deref() {
            Some(fault) => fault.take_call(&mut connection).await,
            None => false,
        };
        let sent = query
            .fetch_one(crate::observed_sql::executor(&mut *connection))
            .await;
        #[cfg(any(test, feature = "testing"))]
        let sent = match sent {
            Ok(_) if loses_answer => Err(sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "injected: the answer of the owner commit was lost",
            ))),
            sent => sent,
        };
        let row = match sent {
            Ok(row) => row,
            Err(error) if declined(&error) => {
                #[cfg(test)]
                if error
                    .as_database_error()
                    .and_then(|error| error.code())
                    .as_deref()
                    == Some("LX003")
                {
                    let _ = FENCE_REFUSALS
                        .try_with(|count| count.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                }
                // The hint a declined head commit planned on is not kept:
                // the owner's next open reads the head again.
                if let Some((key, _)) = &call.hint {
                    self.pools.session_hints.forget(key);
                }
                return Ok(None);
            }
            Err(error) if answered(&error) => return Err(replay::attempt(sqlx_failure(error))),
            Err(lost) => {
                drop(connection);
                if let Some((key, _)) = &call.hint {
                    self.pools.session_hints.forget(key);
                }
                return self
                    .reconcile_call(label, tx, &token, lost, deadline)
                    .await
                    .map(Some);
            }
        };
        // The head the call published is what the owner's next head commit
        // plans on.
        match &call.hint {
            Some((key, Some(after))) => self.pools.session_hints.note(key.clone(), after.clone()),
            Some((key, None)) => self.pools.session_hints.forget(key),
            None => {}
        }
        let fleet: i32 = get(&row, 2).map_err(Attempt::Failed)?;
        if let Ok(fleet) = u32::try_from(fleet) {
            self.fence.observe(fleet);
        }
        let installed: i64 = get(&row, 4).map_err(Attempt::Failed)?;
        if let Some(window) = lash_core_ids::operational_metrics::sql::Window::current() {
            window.installed_graph(u64::try_from(installed).unwrap_or_default());
        }
        let cancel: Vec<Option<String>> = (5..=10)
            .map(|index| get(&row, index))
            .collect::<Result<_, _>>()
            .map_err(Attempt::Failed)?;
        answer(
            tx.actor(),
            &serde_json::json!({
                "revision": get::<i64>(&row, 0).map_err(Attempt::Failed)?,
                "state": get::<String>(&row, 3).map_err(Attempt::Failed)?,
                "cancel": cancel,
                "session_heads": get::<serde_json::Value>(&row, 11).map_err(Attempt::Failed)?,
            }),
        )
        .map(Some)
        .map_err(Attempt::Failed)
    }

    /// Learn whether call `token`, whose answer was `lost`, committed: take
    /// its token unless it recorded it, waiting out a call still running.
    /// Taken: the call never commits, and it runs again. Recorded: its
    /// answer.
    async fn reconcile_call(
        &self,
        label: CommitLabel,
        tx: &ActorTx,
        token: &str,
        lost: sqlx::Error,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<ActorCommit, Attempt<DurableError>> {
        let pool = self.route(label.capacity()).pool;
        let retry = &self.pools.durable_retry;
        let mut last = format!("the connection was lost at the owner commit: {lost}");
        for read in 0..retry.attempts {
            if read > 0 {
                let Some(pause) = retry.pause_within(read - 1, deadline) else {
                    break;
                };
                tokio::time::sleep(pause).await;
            }
            let settled = self.settle_call(pool, token);
            let settled = match deadline {
                Some(deadline) => match tokio::time::timeout_at(deadline, settled).await {
                    Ok(settled) => settled,
                    Err(_) => break,
                },
                None => settled.await,
            };
            match settled {
                Ok(None) => {
                    return Err(Attempt::Aborted(DurableError::Store(StoreFailure {
                        kind: StoreFailureKind::Unavailable,
                        message: format!(
                            "the connection was lost at the owner commit, which never committed: \
                             {lost}"
                        ),
                    })));
                }
                Ok(Some(recorded)) => {
                    return answer(tx.actor(), &recorded).map_err(Attempt::Failed);
                }
                Err(error) => last = format!("its outcome could not be read: {error}"),
            }
        }
        Err(Attempt::Failed(DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Unavailable,
            message: format!("whether owner commit {token} committed is unknown: {last}"),
        })))
    }

    /// Take call `token`'s key, or read the answer it recorded.
    async fn settle_call(
        &self,
        pool: &sqlx::PgPool,
        token: &str,
    ) -> Result<Option<serde_json::Value>, sqlx::Error> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(fault) = self.commit_fault.as_deref()
            && let Some(lost) = fault.outcome_connection(pool).await
        {
            return settle_on(&mut *lost?, token).await;
        }
        let mut connection = crate::observed_sql::checkout(pool).await?;
        settle_on(&mut connection, token).await
    }
}

async fn settle_on(
    connection: &mut sqlx::PgConnection,
    token: &str,
) -> Result<Option<serde_json::Value>, sqlx::Error> {
    let taken = crate::observed_sql::query(EXECUTOR_SQL.latch_preempt.sql())
        .bind(token)
        .fetch_optional(crate::observed_sql::executor(&mut *connection))
        .await?;
    if taken.is_some() {
        return Ok(None);
    }
    let recorded: Option<serde_json::Value> =
        crate::observed_sql::query_scalar(EXECUTOR_SQL.latch_answer.sql())
            .bind(token)
            .fetch_one(crate::observed_sql::executor(&mut *connection))
            .await?;
    Ok(Some(recorded.unwrap_or_default()))
}

/// Decode a recorded answer: the commit's receipt.
pub(super) fn answer(
    actor: &lash_durable::ActorKey,
    recorded: &serde_json::Value,
) -> Result<ActorCommit, DurableError> {
    let corrupt = || super::corrupt("owner commit answer", &recorded.to_string());
    let revision = recorded["revision"].as_i64().ok_or_else(corrupt)?;
    let state = super::actor_state(recorded["state"].as_str().ok_or_else(corrupt)?)?;
    let cancel = recorded["cancel"].as_array().ok_or_else(corrupt)?;
    let text = |index: usize| {
        cancel
            .get(index)
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    };
    let turn_cancel = match (actor.kind(), text(0)) {
        (lash_durable::ActorKind::Session, Some(run)) => {
            let session = lash_kernel_value::identity::SessionId::try_from(actor.id().to_owned())
                .map_err(|_| super::corrupt("session id", actor.id()))?;
            let run = lash_vocabulary::identity::TurnId::try_from(run.clone())
                .map_err(|_| super::corrupt("turn id", &run))?;
            Some(turns::decode_cancel(
                &session,
                &run,
                (
                    text(1).unwrap_or_default(),
                    text(2),
                    text(3),
                    text(4).unwrap_or_default(),
                    text(5).unwrap_or_default(),
                ),
            )?)
        }
        _ => None,
    };
    Ok(ActorCommit {
        process_views: Vec::new(),
        session_heads: recorded["session_heads"]
            .as_array()
            .ok_or_else(corrupt)?
            .iter()
            .map(|head| {
                let session =
                    serde_json::from_value(head["session"].clone()).map_err(|_| corrupt())?;
                let receipt =
                    serde_json::from_value(head["receipt"].clone()).map_err(|_| corrupt())?;
                Ok(lash_durable::CommittedSessionHead { session, receipt })
            })
            .collect::<Result<_, DurableError>>()?,
        root_leases: Vec::new(),
        revision: lash_durable::StateRevision(revision),
        state,
        turn_cancel,
    })
}

#[cfg(test)]
tokio::task_local! {
    static FENCE_REFUSALS: std::sync::atomic::AtomicUsize;
}

#[cfg(test)]
pub(super) async fn observe_fence_refusals<T>(
    future: impl std::future::Future<Output = T>,
) -> (T, usize) {
    FENCE_REFUSALS
        .scope(std::sync::atomic::AtomicUsize::new(0), async {
            let answer = future.await;
            (
                answer,
                FENCE_REFUSALS.with(|count| count.load(std::sync::atomic::Ordering::Relaxed)),
            )
        })
        .await
}
