//! Turn phase state, the session commit and turn cancel requests on SQLite: the `turns` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L3 (FIG-5172). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back.

use std::sync::LazyLock;

use lash_core_store::store::HeadWriter;
use lash_core_store::store::RunAdmissionRecord;
use lash_core_store::store::RunTerminalCause;
use lash_core_store::store_backend_support::turn_cancel::turn_cancel_mode_from_wire;
use lash_core_store::store_backend_support::turn_cancel::turn_cancel_mode_wire;
use lash_core_store::store_backend_support::turn_cancel::turn_cancel_undelivered_from_wire;
use lash_core_store::store_backend_support::turn_cancel::turn_cancel_undelivered_wire;
use lash_durable::domain::namespace_value::NamespaceValue;
use lash_durable::domain::{
    DomainRefusal, ExecKey, SessionCommitWrite, StoredValues, TurnCancelAnswer, TurnCancelRequest,
    TurnEnd, TurnNamespace, TurnRow, TurnWrite, UnfinishedPhase,
};
use lash_durable::{ActorKey, DurableError, DurableInstant, Epoch, Woken};
use lash_kernel_value::identity::SessionId;
use lash_store_sql::durable::turns::TurnStatements;
use lash_vocabulary::identity::TurnId;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, corrupt, integer, node_ref};
use crate::conn::cached_execute;

/// `turn_phases`: V0's (FIG-5170) side table of `session_runs`. Each phase
/// row carries exactly what its phase restores from (FIG-5221): no
/// checkpoint while admitted and one after, and the model pin exactly in the
/// model phase, whose attempt is the phase argument and whose call is
/// `model_calls`, the count of model calls the turn admitted, and the pinned
/// call's receipt only in the model phase.
pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS turn_phases (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    phase TEXT NOT NULL CONSTRAINT ck_turn_phases_phase
        CHECK (phase IN ('admitted', 'model', 'tools')),
    phase_arg INTEGER,
    iteration INTEGER NOT NULL,
    -- The phase's PhaseRoot node and the post-cell working SessionExecution
    -- (FIG-5887 §6, §7), and the pinned call's receipt: its placed text,
    -- from model.answer until the next phase.
    checkpoint_ref BLOB REFERENCES blobs(hash) ON DELETE RESTRICT,
    working_execution_ref BLOB REFERENCES blobs(hash) ON DELETE RESTRICT,
    answer_ref BLOB REFERENCES blobs(hash) ON DELETE RESTRICT,
    model_request_ref TEXT,
    model_deadline_ms INTEGER,
    model_stream_from TEXT,
    turn_deadline_ms INTEGER,
    written_epoch INTEGER NOT NULL,
    model_calls INTEGER NOT NULL DEFAULT 0,
    tool_calls INTEGER NOT NULL DEFAULT 0 CHECK (tool_calls >= 0),
    PRIMARY KEY (session_id, run),
    CONSTRAINT ck_turn_phases_arg CHECK ((phase IN ('model', 'tools')) = (phase_arg IS NOT NULL)),
    CONSTRAINT ck_turn_phases_checkpoint CHECK ((phase = 'admitted') = (checkpoint_ref IS NULL)),
    CONSTRAINT ck_turn_phases_answer CHECK (answer_ref IS NULL OR phase = 'model'),
    CONSTRAINT ck_turn_phases_model CHECK (
        (phase = 'model') = (model_request_ref IS NOT NULL)
        AND (phase = 'model') = (model_deadline_ms IS NOT NULL)
        AND (phase = 'model') = (model_stream_from IS NOT NULL))
) WITHOUT ROWID;

-- The plugin namespaces an unfinished turn's run changed (FIG-5301): the
-- entry (values address and metadata, JSON) and its values, dropped with the
-- phase row when the turn ends.
CREATE TABLE IF NOT EXISTS turn_namespaces (
    session_id TEXT NOT NULL,
    run TEXT NOT NULL,
    plugin TEXT NOT NULL,
    entry TEXT NOT NULL,
    -- The namespace's NamespaceValue node; NULL exactly for base values
    -- (FIG-5887 §7).
    graph_root_ref BLOB REFERENCES blobs(hash) ON DELETE RESTRICT,
    PRIMARY KEY (session_id, run, plugin)
);
CREATE INDEX IF NOT EXISTS idx_turn_phases_checkpoint_ref ON turn_phases (checkpoint_ref);
CREATE INDEX IF NOT EXISTS idx_turn_phases_working_execution_ref
    ON turn_phases (working_execution_ref);
CREATE INDEX IF NOT EXISTS idx_turn_phases_answer_ref ON turn_phases (answer_ref);
CREATE INDEX IF NOT EXISTS idx_turn_namespaces_graph_root_ref ON turn_namespaces (graph_root_ref);
";

static SQL: LazyLock<TurnStatements> =
    LazyLock::new(|| TurnStatements::render(crate::schema_layout::MAIN));

lash_store_sql::statements! {
    /// SQLite avoids an index update when the old root is already NULL.
    struct SqliteTurnStatements @ "durable_turn_sqlite" {
        /// Advance without rewriting an already empty answer root.
        advance_without_answer = "UPDATE turn_phases
             SET phase = ?3, phase_arg = ?4, iteration = ?5, checkpoint_ref = ?6,
                 model_request_ref = ?7, model_deadline_ms = ?8, written_epoch = ?9,
                 model_calls = COALESCE(?10, model_calls), model_stream_from = ?11
             WHERE session_id = ?1 AND run = ?2
             RETURNING run";
        /// End without rewriting the transcript root, which is NULL for an
        /// unfinished run. The typed terminal has no transcript in this shape.
        end_without_transcript = "UPDATE session_runs
             SET terminal_kind = ?3, terminal_cause_json = ?4,
                 terminal_head_revision = ?5, terminal_at_ms = ?6,
                 terminal_window_revision = (
                     SELECT revision.head_revision FROM session_head AS head
                     JOIN session_revisions AS revision
                       ON revision.session_id = head.session_id
                      AND revision.head_revision = head.head_revision
                     WHERE head.session_id = ?1)
             WHERE session_id = ?1 AND run = ?2
               AND admission_json IS NOT NULL AND terminal_kind IS NULL AND ?7 IS NULL
             RETURNING run";
    }
}
static SQLITE_SQL: LazyLock<SqliteTurnStatements> =
    LazyLock::new(|| SqliteTurnStatements::render(crate::schema_layout::MAIN));

fn refuse<T>(refusal: DomainRefusal) -> Answer<T> {
    Ok(Err(DurableError::Domain(refusal)))
}

/// A store codec's refusal, as the durable port reports it.
fn encoding(error: &lash_core_store::store::StoreError) -> DurableError {
    DurableError::Store(lash_durable::StoreFailure {
        kind: lash_durable::StoreFailureKind::Corrupt,
        message: error.to_string(),
    })
}

pub(super) fn apply(
    tx: &crate::conn::FencedTx<'_>,
    commit: &Committing<'_>,
    write: &TurnWrite,
) -> Answer<()> {
    // Every write but an advance (which takes them) and a tool-call count
    // may change the phase row the fenced read saw.
    if !matches!(
        write,
        TurnWrite::Advance { .. } | TurnWrite::AddToolCalls { .. }
    ) {
        commit.open_roots.forget();
    }
    match write {
        TurnWrite::Admit {
            session,
            run,
            admission,
            turn_deadline,
        } => {
            #[cfg(feature = "perf-witness")]
            lash_core_ids::perf_witness::latency::stage(|| {
                lash_core_ids::perf_witness::latency::CommitMark::Admitted {
                    session: session.as_str().to_owned(),
                    run: run.as_str().to_owned(),
                    inputs: admission
                        .input_ids()
                        .iter()
                        .map(|id| id.as_str().to_owned())
                        .collect(),
                }
            });
            let admission = match admission.to_stored() {
                Ok(admission) => admission,
                Err(error) => return Ok(Err(encoding(&error))),
            };
            let admitted = tx
                .prepare_cached(SQL.admit_run.sql())?
                .query_row(
                    rusqlite::params![session.as_str(), run.as_str(), admission],
                    |_| Ok(()),
                )
                .optional()?;
            if admitted.is_none() {
                return refuse(DomainRefusal::OpenTurnExists {
                    session: session.clone(),
                });
            }
            let (phase, argument) = UnfinishedPhase::Admitted.stored();
            cached_execute(
                tx,
                SQL.insert_phase.sql(),
                rusqlite::params![
                    session.as_str(),
                    run.as_str(),
                    phase,
                    argument.map(integer::<i64>).transpose()?,
                    0_i64,
                    Option::<String>::None,
                    Option::<String>::None,
                    Option::<i64>::None,
                    turn_deadline.map(|deadline| deadline.0),
                    commit.epoch.0,
                ],
            )?;
            *commit.open_roots.0.borrow_mut() = Some(PhaseRoots {
                session: session.as_str().to_owned(),
                run: run.as_str().to_owned(),
                checkpoint: None,
                answer: None,
            });
            Ok(Ok(()))
        }
        TurnWrite::Advance {
            session,
            run,
            phase,
            iteration,
        } => {
            let (stored, argument) = phase.stored();
            let pin = phase.model();
            // The phase's PhaseRoot it replaces and the receipt it consumes,
            // peeled once the commit's writes are applied: as the commit's
            // fenced turn read saw them while no write of this transaction
            // changed the phase row since, else read again.
            let (replaced, consumed) = match commit.open_roots.take(session, run) {
                Some(roots) => roots,
                None => tx
                    .prepare_cached(SQL.phase_roots.sql())?
                    .query_row([session.as_str(), run.as_str()], |row| {
                        Ok((
                            crate::codec::optional_hash_column(row, 0)?,
                            crate::codec::optional_hash_column(row, 1)?,
                        ))
                    })
                    .optional()?
                    .unwrap_or_default(),
            };
            let advanced = tx
                .prepare_cached(if consumed.is_some() {
                    SQL.advance_phase.sql()
                } else {
                    SQLITE_SQL.advance_without_answer.sql()
                })?
                .query_row(
                    rusqlite::params![
                        session.as_str(),
                        run.as_str(),
                        stored,
                        argument.map(integer::<i64>).transpose()?,
                        i64::from(*iteration),
                        phase.checkpoint().0.as_slice(),
                        pin.map(|pin| pin.request_ref.as_str()),
                        pin.map(|pin| pin.deadline.0),
                        commit.epoch.0,
                        pin.map(|pin| i64::from(pin.call)),
                        pin.map(|pin| pin.stream_from.as_str()),
                    ],
                    |_| Ok(()),
                )
                .optional()?;
            match advanced {
                Some(()) => {
                    let next = phase.checkpoint().to_hex();
                    if let Some(old) = replaced.filter(|old| *old != next) {
                        commit.retired.stored(old);
                    }
                    if let Some(answer) = consumed {
                        commit.retired.stored(answer);
                    }
                    Ok(Ok(()))
                }
                None => refuse(DomainRefusal::TurnNotOpen {
                    session: session.clone(),
                    run: run.clone(),
                }),
            }
        }
        TurnWrite::AddToolCalls {
            session,
            run,
            calls,
        } => {
            tx.prepare_cached(SQL.add_tool_calls.sql())?
                .execute(rusqlite::params![
                    session.as_str(),
                    run.as_str(),
                    integer::<i64>(*calls)?,
                ])?;
            Ok(Ok(()))
        }
        TurnWrite::Answer {
            session,
            run,
            answer,
        } => {
            let recorded = tx
                .prepare_cached(SQL.answer_set.sql())?
                .query_row(
                    rusqlite::params![
                        session.as_str(),
                        run.as_str(),
                        answer.0.as_slice(),
                        commit.epoch.0
                    ],
                    |_| Ok(()),
                )
                .optional()?;
            match recorded {
                Some(()) => Ok(Ok(())),
                None => refuse(DomainRefusal::TurnNotOpen {
                    session: session.clone(),
                    run: run.clone(),
                }),
            }
        }
        TurnWrite::Namespaces {
            session,
            run,
            namespaces,
        } => {
            let open = tx
                .prepare_cached(SQL.open_named_run.sql())?
                .query_row([session.as_str(), run.as_str()], |_| Ok(()))
                .optional()?;
            if open.is_none() {
                return refuse(DomainRefusal::TurnNotOpen {
                    session: session.clone(),
                    run: run.clone(),
                });
            }
            for namespace in namespaces {
                let entry = match serde_json::to_string(&namespace.entry) {
                    Ok(entry) => entry,
                    Err(error) => {
                        return Ok(Err(corrupt("turn namespace entry", &error.to_string())));
                    }
                };
                let values = match &namespace.values {
                    StoredValues::Root(root) => Some(root.to_hex()),
                    StoredValues::Base | StoredValues::Held => None,
                };
                // A held write keeps the stored root; any other replaces it.
                if !matches!(namespace.values, StoredValues::Held) {
                    let replaced = tx
                        .prepare_cached(SQL.namespace_root.sql())?
                        .query_row(
                            [session.as_str(), run.as_str(), namespace.plugin.as_str()],
                            |row| crate::codec::optional_hash_column(row, 0),
                        )
                        .optional()?
                        .flatten();
                    if let Some(old) = replaced.filter(|old| Some(old) != values.as_ref()) {
                        commit.retired.stored(old);
                    }
                }
                cached_execute(
                    tx,
                    SQL.upsert_namespace.sql(),
                    rusqlite::params![
                        session.as_str(),
                        run.as_str(),
                        namespace.plugin,
                        entry,
                        values.as_deref().map(crate::codec::SqlHash),
                        matches!(namespace.values, StoredValues::Held),
                    ],
                )?;
            }
            Ok(Ok(()))
        }
        TurnWrite::Working {
            session,
            run,
            expected,
            working,
        } => {
            let set = tx
                .prepare_cached(SQL.working_set.sql())?
                .query_row(
                    rusqlite::params![
                        session.as_str(),
                        run.as_str(),
                        working.map(|working| working.0),
                        expected.map(|expected| expected.0),
                        commit.epoch.0,
                    ],
                    |_| Ok(()),
                )
                .optional()?;
            match set {
                Some(()) => {
                    if let Some(old) = expected.filter(|old| Some(*old) != *working) {
                        commit.retired.closure(&old);
                    }
                    Ok(Ok(()))
                }
                None => refuse(DomainRefusal::TurnNotOpen {
                    session: session.clone(),
                    run: run.clone(),
                }),
            }
        }
        TurnWrite::Terminal {
            session,
            run,
            cause,
            head_revision,
            transcript,
        } => {
            #[cfg(feature = "perf-witness")]
            lash_core_ids::perf_witness::latency::stage(|| {
                lash_core_ids::perf_witness::latency::CommitMark::Terminal {
                    run: run.as_str().to_owned(),
                }
            });
            let mut cause = cause.as_ref().clone();
            if let RunTerminalCause::Cancelled {
                evidence,
                input_outcome,
            } = &mut cause
            {
                *input_outcome = match super::session_mail::cancel_inputs(
                    tx,
                    session,
                    run,
                    evidence.undelivered,
                )? {
                    Ok(outcome) => outcome,
                    Err(error) => return Ok(Err(error)),
                };
            }
            let stored = match cause.to_stored() {
                Ok(stored) => stored,
                Err(error) => return Ok(Err(encoding(&error))),
            };
            let ended = tx
                .prepare_cached(if transcript.is_some() {
                    SQL.end_run.sql()
                } else {
                    SQLITE_SQL.end_without_transcript.sql()
                })?
                .query_row(
                    rusqlite::params![
                        session.as_str(),
                        run.as_str(),
                        cause.kind().as_str(),
                        stored,
                        head_revision.map(integer::<i64>).transpose()?,
                        commit.now.0,
                        transcript.map(|transcript| transcript.0),
                    ],
                    |_| Ok(()),
                )
                .optional()?;
            if ended.is_none() {
                return refuse(DomainRefusal::TurnNotOpen {
                    session: session.clone(),
                    run: run.clone(),
                });
            }
            if let Some(revision) = head_revision {
                crate::revisions::record_terminal_conn(tx, session, *revision, commit.now)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            }
            // The turn's phase row and namespaces go with it, and their
            // roots with them: its working execution, receipt and namespace
            // values whole, its PhaseRoot alone (the TurnContent ordering
            // constraint, [`super::Retired`]).
            let phase = tx
                .prepare_cached(SQL.delete_phase.sql())?
                .query_row([session.as_str(), run.as_str()], |row| {
                    Ok((
                        crate::codec::optional_hash_column(row, 0)?,
                        crate::codec::optional_hash_column(row, 1)?,
                        crate::codec::optional_hash_column(row, 2)?,
                    ))
                })
                .optional()?;
            if let Some((checkpoint, working, answer)) = phase {
                if let Some(checkpoint) = checkpoint {
                    commit.retired.phase_end(checkpoint);
                }
                for root in [working, answer].into_iter().flatten() {
                    commit.retired.stored(root);
                }
            }
            let namespaces = tx
                .prepare_cached(SQL.delete_namespaces.sql())?
                .query_map([session.as_str(), run.as_str()], |row| {
                    crate::codec::optional_hash_column(row, 0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for root in namespaces.into_iter().flatten() {
                commit.retired.stored(root);
            }
            super::session_mail::settle_held(tx, session, run, commit.now.0)?;
            crate::obligation_ledger::end_execution_tx(
                tx,
                &lash_protocol_types::ExecutionScope::turn(session.clone(), run.clone()),
                integer(commit.now.0)?,
            )
            .map_err(crate::sqlite_conversion_error)?;
            super::packages::end_actor_executions(tx, commit.actor)?;
            // The cells the turn stopped in are never resumed, and the
            // turn's cancel request is answered by its terminal: neither has
            // a reader left. Nor has the turn's own ledger: its transcript
            // names the material its answered calls are read from.
            for root in super::snapshots::retire_under(tx, &ExecKey::turn_cells(session, run))? {
                commit.retired.stored(root);
            }
            let owner = lash_durable::domain::OwnerKey::Turn(session.clone(), run.clone());
            for record in super::run_records::delete_owner(tx, &owner.stored())? {
                commit.retired.stored(record);
            }
            cached_execute(
                tx,
                SQL.delete_cancel.sql(),
                rusqlite::params![session.as_str(), run.as_str()],
            )?;
            Ok(Ok(()))
        }
    }
}

/// Request-only decoding and identity work, before writer enqueue. Its
/// refusal is returned only when the fenced domain application reaches it.
pub(super) fn prepare_session_commit(
    write: &SessionCommitWrite,
    fleet: lash_core_store::store::FleetFormat,
) -> Result<lash_core_store::store::RuntimeCommitPlanner, DurableError> {
    let runtime_commit = match lash_core_store::store::decode_session_commit(&write.commit_json) {
        Ok(runtime_commit) => runtime_commit,
        Err(error) => return Err(DurableError::session_commit(write.session.clone(), &error)),
    };
    if runtime_commit.session_id != write.session
        || runtime_commit.expected_head_revision != write.expected_head
    {
        return Err(DurableError::Domain(DomainRefusal::SessionCommitRefused {
            session: write.session.clone(),
            code: lash_core_store::runtime_error::RuntimeErrorCode::StoreRefused,
            cause: None,
            reason: format!(
                "the commit names session {} at head {}, not {} at {}",
                runtime_commit.session_id,
                runtime_commit.expected_head_revision,
                write.session,
                write.expected_head
            ),
        }));
    }
    let planner = match lash_core_store::store::RuntimeCommitPlanner::prepare(runtime_commit, fleet)
    {
        Ok(planner) => planner,
        Err(error) => return Err(DurableError::session_commit(write.session.clone(), &error)),
    };
    Ok(planner)
}

pub(super) fn apply_session_commit(
    tx: &crate::conn::FencedTx<'_>,
    commit: &Committing<'_>,
    write: &SessionCommitWrite,
    prepared: &Result<lash_core_store::store::RuntimeCommitPlanner, DurableError>,
    planned_under: lash_core_store::store::FleetFormat,
) -> Answer<lash_core_store::store::RuntimeCommitReceipt> {
    commit.open_roots.forget();
    // A fleet move changes the versions a plan emits. Reclassify against
    // the fleet read by this transaction, just as the public store does.
    let replanned;
    let prepared = if tx.fleet() == planned_under {
        prepared
    } else {
        replanned = prepare_session_commit(write, tx.fleet());
        &replanned
    };
    let planner = match prepared {
        Ok(planner) => planner,
        Err(error) => return Ok(Err(error.clone())),
    };
    let refused = |error: &lash_core_store::store::StoreError| {
        Ok(Err(DurableError::session_commit(
            write.session.clone(),
            error,
        )))
    };
    let now = integer::<u64>(commit.now.0)?;
    match crate::persistence::session_commit::apply_runtime_commit_conn(
        tx,
        planner,
        head_writer(commit.actor, &write.session),
        commit.blob_profile,
        now,
    ) {
        Ok(receipt) => Ok(Ok(receipt)),
        Err(error) => refused(&error),
    }
}

/// Who writes a session head commit fenced by `actor`: the session's own
/// actor, whose epoch fences every head write it makes (FIG-5355), or any
/// other owner, which the head's ownership gate checks as a store writer.
fn head_writer(actor: &ActorKey, session: &SessionId) -> HeadWriter {
    if ActorKey::session(session.as_str()).is_ok_and(|own| &own == actor) {
        HeadWriter::SessionActor
    } else {
        HeadWriter::Store
    }
}

pub(super) fn request_cancel(
    tx: &Connection,
    request: &TurnCancelRequest,
    now: DurableInstant,
) -> Answer<(TurnCancelAnswer, Option<Woken>)> {
    let session = request.session.as_str();
    let run = request.run.as_str();
    let actor = match ActorKey::session(session) {
        Ok(actor) => actor,
        Err(_) => return Ok(Err(corrupt("session id", session))),
    };
    // A turn no run opened yet is its queued input: the withdraw takes the
    // open row first, and an admission that bound it first leaves the open
    // run this request cancels (FIG-5262).
    let withdrawn =
        match super::session_mail::withdraw_queued(tx, &request.session, &request.run, now.0)? {
            Ok(withdrawn) => withdrawn,
            Err(error) => return Ok(Err(error)),
        };
    if let Some(input) = withdrawn {
        return Ok(super::wake_within(tx, &actor, false, now)?
            .map(|(woken, _)| (TurnCancelAnswer::Withdrawn { input }, Some(woken))));
    }
    let open = tx
        .prepare_cached(SQL.open_named_run.sql())?
        .query_row([session, run], |_| Ok(()))
        .optional()?;
    if open.is_none() {
        return Ok(Ok((TurnCancelAnswer::AlreadyEnded, None)));
    }
    let accepted = match cancel_of(tx, &request.session, &request.run)? {
        Ok(accepted) => accepted,
        Err(error) => return Ok(Err(error)),
    };
    let answer = match accepted {
        None => {
            cached_execute(
                tx,
                SQL.insert_cancel.sql(),
                rusqlite::params![
                    session,
                    run,
                    request.request_id,
                    request.origin,
                    request.reason,
                    turn_cancel_undelivered_wire(request.undelivered),
                    turn_cancel_mode_wire(request.mode),
                ],
            )?;
            TurnCancelAnswer::Requested
        }
        Some(mut accepted) if request.escalates(&accepted) => {
            cached_execute(
                tx,
                SQL.escalate_cancel.sql(),
                rusqlite::params![
                    session,
                    run,
                    request.request_id,
                    request.origin,
                    request.reason,
                    turn_cancel_mode_wire(request.mode),
                ],
            )?;
            accepted.escalate_to(request);
            TurnCancelAnswer::Escalated { accepted }
        }
        Some(accepted) if accepted.undelivered != request.undelivered => {
            return Ok(Ok((TurnCancelAnswer::PolicyConflict { accepted }, None)));
        }
        Some(accepted) => {
            return Ok(Ok((TurnCancelAnswer::AlreadyRequested { accepted }, None)));
        }
    };
    Ok(super::wake_within(tx, &actor, true, now)?.map(|(woken, _)| (answer, Some(woken))))
}

/// The cancel request run `run` of `session` accepted.
fn cancel_of(
    tx: &Connection,
    session: &SessionId,
    run: &TurnId,
) -> Answer<Option<TurnCancelRequest>> {
    let stored = tx
        .prepare_cached(SQL.cancel_of.sql())?
        .query_row([session.as_str(), run.as_str()], |row| {
            StoredCancel::read(row, 0)
        })
        .optional()?;
    Ok(stored.map(|stored| stored.decode(session, run)).transpose())
}

/// A turn's accepted cancel request as stored: request id, origin, reason,
/// disposition and mode.
pub(super) struct StoredCancel(String, Option<String>, Option<String>, String, String);

impl StoredCancel {
    /// The five columns from `first` on.
    pub(super) fn read(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<Self> {
        Ok(Self(
            row.get(first)?,
            row.get(first + 1)?,
            row.get(first + 2)?,
            row.get(first + 3)?,
            row.get(first + 4)?,
        ))
    }

    pub(super) fn decode(
        self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<TurnCancelRequest, DurableError> {
        let Self(request_id, origin, reason, disposition, mode) = self;
        let (Ok(undelivered), Ok(mode)) = (
            turn_cancel_undelivered_from_wire(&disposition),
            turn_cancel_mode_from_wire(&mode),
        ) else {
            return Err(corrupt(
                "turn cancel request",
                &format!("{disposition}/{mode}"),
            ));
        };
        Ok(TurnCancelRequest {
            session: session.clone(),
            run: run.clone(),
            request_id,
            origin,
            reason,
            undelivered,
            mode,
        })
    }
}

pub(super) fn turn_end(
    tx: &Connection,
    session: &SessionId,
    run: &TurnId,
) -> Answer<Option<TurnEnd>> {
    let stored = tx
        .prepare_cached(SQL.ended.sql())?
        .query_row([session.as_str(), run.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
        })
        .optional()?;
    let Some((cause, head_revision)) = stored else {
        return Ok(Ok(None));
    };
    let cause = match RunTerminalCause::from_stored(&cause) {
        Ok(cause) => cause,
        Err(error) => return Ok(Err(encoding(&error))),
    };
    let head_revision = head_revision.map(integer::<u64>).transpose()?;
    Ok(Ok(Some(TurnEnd {
        cause,
        head_revision,
    })))
}

pub(super) fn turn_namespaces(
    tx: &Connection,
    session: &SessionId,
    run: &TurnId,
    limits: lash_core_store::checkpoint_graph::DecodeLimits,
) -> Answer<Vec<TurnNamespace>> {
    let mut statement = tx.prepare_cached(SQL.namespaces.sql())?;
    let rows = statement
        .query_map([session.as_str(), run.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                crate::codec::optional_hash_column(row, 2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut namespaces = Vec::with_capacity(rows.len());
    for (plugin, entry, root) in rows {
        let Ok(entry) = serde_json::from_str(&entry) else {
            return Ok(Err(corrupt("turn namespace entry", &plugin)));
        };
        let body = match root {
            None => None,
            Some(root) => {
                let root = match node_ref(&root) {
                    Ok(root) => root,
                    Err(error) => return Ok(Err(error)),
                };
                let nodes = match super::graph::graph_closure(tx, root, &limits)? {
                    Ok(nodes) => nodes,
                    Err(refusal) => return Ok(Err(DurableError::from(refusal))),
                };
                match NamespaceValue::load(&nodes, root, &plugin, limits) {
                    Ok(value) => Some(value.body),
                    Err(refusal) => return Ok(Err(DurableError::from(refusal))),
                }
            }
        };
        namespaces.push(TurnNamespace {
            plugin,
            entry,
            body,
        });
    }
    Ok(Ok(namespaces))
}

/// Session `session`'s unfinished turn with the cancel it accepted, read in
/// one statement.
pub(super) fn turn(tx: &Connection, session: &SessionId) -> Answer<Option<TurnRow>> {
    Ok(turn_with_roots(tx, session)?.map(|turn| turn.map(|(row, _)| row)))
}

/// [`turn`], with the phase row's roots as stored.
pub(super) fn turn_with_roots(
    tx: &Connection,
    session: &SessionId,
) -> Answer<Option<(TurnRow, PhaseRoots)>> {
    let stored = tx
        .prepare_cached(super::SQL.sqlite.turn.sql())?
        .query_row([session.as_str()], |row| {
            Ok((
                StoredTurn::read(row, 0)?,
                row.get::<_, Option<String>>(15)?
                    .map(|_| StoredCancel::read(row, 15))
                    .transpose()?,
            ))
        })
        .optional()?;
    let Some((stored, cancel)) = stored else {
        return Ok(Ok(None));
    };
    let roots = PhaseRoots {
        session: session.as_str().to_owned(),
        run: stored.run.clone(),
        checkpoint: stored.checkpoint.clone(),
        answer: stored.answer.clone(),
    };
    Ok(stored
        .decode(session, |run| {
            Ok(cancel.map(|cancel| cancel.decode(session, run)).transpose())
        })?
        .map(|row| Some((row, roots))))
}

/// Read transaction guards without decoding acknowledged admission/phase state.
pub(super) fn turn_frontier(
    tx: &Connection,
    session: &SessionId,
) -> Answer<Option<(Option<TurnCancelRequest>, PhaseRoots)>> {
    let stored = tx
        .prepare_cached(super::SQL.sqlite.turn_frontier.sql())?
        .query_row([session.as_str()], |row| {
            StoredFrontier::read(row, 0, session.as_str())
        })
        .optional()?
        .flatten();
    Ok(stored.map(|stored| stored.decode(session)).transpose())
}

pub(super) struct StoredFrontier {
    roots: PhaseRoots,
    cancel: Option<StoredCancel>,
}

impl StoredFrontier {
    pub(super) fn read(
        row: &rusqlite::Row<'_>,
        first: usize,
        session: &str,
    ) -> rusqlite::Result<Option<Self>> {
        let Some(run) = row.get::<_, Option<String>>(first)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            roots: PhaseRoots {
                session: session.to_owned(),
                run,
                checkpoint: crate::codec::optional_hash_column(row, first + 1)?,
                answer: crate::codec::optional_hash_column(row, first + 2)?,
            },
            cancel: row
                .get::<_, Option<String>>(first + 3)?
                .map(|_| StoredCancel::read(row, first + 3))
                .transpose()?,
        }))
    }

    pub(super) fn decode(
        self,
        session: &SessionId,
    ) -> Result<(Option<TurnCancelRequest>, PhaseRoots), DurableError> {
        let run = TurnId::try_from(self.roots.run.clone())
            .map_err(|error| corrupt("turn run", &error.to_string()))?;
        let cancel = self
            .cancel
            .map(|cancel| cancel.decode(session, &run))
            .transpose()?;
        Ok((cancel, self.roots))
    }
}

/// An unfinished turn's phase root and receipt as stored.
pub(super) struct PhaseRoots {
    session: String,
    run: String,
    checkpoint: Option<String>,
    answer: Option<String>,
}

/// The open turn's phase root and receipt as an owner commit's fenced turn
/// read saw them, before its writes (FIG-6617). Only this module's writes
/// change a phase row, and the transaction holds the database's write lock,
/// so they hold until a write of this commit changes the row: any turn
/// write but a tool-call count forgets them, and an advance takes them.
#[derive(Default)]
pub(crate) struct OpenRoots(std::cell::RefCell<Option<PhaseRoots>>);

impl OpenRoots {
    /// The roots `roots` names, read under the commit's fence.
    pub(super) fn read(roots: Option<PhaseRoots>) -> Self {
        Self(std::cell::RefCell::new(roots))
    }

    /// Take the roots of run `run` of `session`, when they are still held.
    fn take(&self, session: &SessionId, run: &TurnId) -> Option<(Option<String>, Option<String>)> {
        let mut held = self.0.borrow_mut();
        if held
            .as_ref()
            .is_some_and(|roots| roots.session == session.as_str() && roots.run == run.as_str())
        {
            held.take().map(|roots| (roots.checkpoint, roots.answer))
        } else {
            None
        }
    }

    fn forget(&self) {
        self.0.borrow_mut().take();
    }
}

/// A session's unfinished turn as stored: its run row and phase row, in the
/// column order of the `unfinished` statement.
pub(super) struct StoredTurn {
    run: String,
    admission: String,
    phase: String,
    argument: Option<i64>,
    iteration: i64,
    checkpoint: Option<String>,
    working: Option<String>,
    answer: Option<String>,
    request: Option<String>,
    deadline: Option<i64>,
    turn_deadline: Option<i64>,
    epoch: i64,
    model_calls: i64,
    tool_calls: i64,
    stream_from: Option<String>,
}

impl StoredTurn {
    /// The fifteen columns from `first` on.
    pub(super) fn read(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            run: row.get(first)?,
            admission: row.get(first + 1)?,
            phase: row.get(first + 2)?,
            argument: row.get(first + 3)?,
            iteration: row.get(first + 4)?,
            checkpoint: crate::codec::optional_hash_column(row, first + 5)?,
            working: crate::codec::optional_hash_column(row, first + 12)?,
            answer: crate::codec::optional_hash_column(row, first + 13)?,
            request: row.get(first + 6)?,
            deadline: row.get(first + 7)?,
            turn_deadline: row.get(first + 8)?,
            epoch: row.get(first + 9)?,
            model_calls: row.get(first + 10)?,
            tool_calls: row.get(first + 14)?,
            stream_from: row.get(first + 11)?,
        })
    }

    /// The turn, refusing a corrupt column in column order, and then its
    /// accepted cancel from `cancel`.
    pub(super) fn decode(
        self,
        session: &SessionId,
        cancel: impl FnOnce(&TurnId) -> Answer<Option<TurnCancelRequest>>,
    ) -> Answer<TurnRow> {
        let Ok(run) = TurnId::try_from(self.run.clone()) else {
            return Ok(Err(corrupt("turn id", &self.run)));
        };
        let admission = match RunAdmissionRecord::from_stored(&self.admission) {
            Ok(admission) => admission,
            Err(error) => return Ok(Err(encoding(&error))),
        };
        let argument = self.argument.map(integer::<u64>).transpose()?;
        let iteration = integer::<u32>(self.iteration)?;
        let model_calls = integer::<u32>(self.model_calls)?;
        let pin = self
            .request
            .zip(self.deadline.map(DurableInstant))
            .zip(self.stream_from)
            .map(|((request, deadline), stream_from)| (request, deadline, stream_from));
        let checkpoint = match self.checkpoint.as_deref().map(node_ref).transpose() {
            Ok(checkpoint) => checkpoint,
            Err(error) => return Ok(Err(error)),
        };
        let working = match self.working.as_deref().map(node_ref).transpose() {
            Ok(working) => working,
            Err(error) => return Ok(Err(error)),
        };
        let answer = match self.answer.as_deref().map(node_ref).transpose() {
            Ok(answer) => answer,
            Err(error) => return Ok(Err(error)),
        };
        let Some(phase) =
            UnfinishedPhase::parse(&self.phase, argument, checkpoint, pin, model_calls, answer)
        else {
            return Ok(Err(corrupt("turn phase", &self.phase)));
        };
        let tool_calls = integer::<u64>(self.tool_calls)?;
        let cancel = match cancel(&run)? {
            Ok(cancel) => cancel,
            Err(error) => return Ok(Err(error)),
        };
        Ok(Ok(TurnRow {
            session: session.clone(),
            run,
            admission,
            phase,
            working,
            iteration,
            model_calls,
            tool_calls,
            turn_deadline: self.turn_deadline.map(DurableInstant),
            written_epoch: Epoch(self.epoch),
            cancel,
        }))
    }
}

#[cfg(test)]
mod ddl_tests {
    //! The storage laws of a turn's stored forms (FIG-5221): the DDL itself
    //! refuses a phase row without what its phase restores from, and a run
    //! terminal whose kind its cause does not derive.

    use rusqlite::{Connection, params};

    fn tables() -> Connection {
        let conn = Connection::open_in_memory().expect("an in-memory database");
        conn.execute_batch(crate::schema::SCHEMA).expect("blobs");
        conn.execute(
            "INSERT INTO blobs (hash, content, encoding) VALUES ('{}', x'00', 1)",
            [],
        )
        .expect("a checkpoint blob");
        conn.execute_batch(super::TABLES).expect("turn_phases");
        conn.execute_batch(crate::schema_fragments::SESSION_RUNS_TABLES)
            .expect("session_runs");
        conn
    }

    fn phase(
        conn: &Connection,
        run: &str,
        phase: &str,
        argument: Option<i64>,
        checkpoint: Option<&str>,
        pin: Option<(&str, i64)>,
    ) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT INTO turn_phases (session_id, run, phase, phase_arg, iteration,
                 checkpoint_ref, model_request_ref, model_deadline_ms, model_stream_from,
                 written_epoch)
             VALUES ('s', ?1, ?2, ?3, 0, ?4, ?5, ?6, ?7, 1)",
            params![
                run,
                phase,
                argument,
                checkpoint,
                pin.map(|(request, _)| request),
                pin.map(|(_, deadline)| deadline),
                pin.map(|_| "stream-cursor")
            ],
        )
    }

    #[test]
    fn the_ddl_refuses_a_model_phase_without_its_pin() {
        let conn = tables();
        assert!(phase(&conn, "unpinned", "model", Some(1), Some("{}"), None).is_err());
        assert!(
            phase(
                &conn,
                "tools-pinned",
                "tools",
                Some(1),
                Some("{}"),
                Some(("r", 5))
            )
            .is_err()
        );
        phase(
            &conn,
            "pinned",
            "model",
            Some(1),
            Some("{}"),
            Some(("r", 5)),
        )
        .expect("a pinned model phase is stored");
    }

    #[test]
    fn the_ddl_refuses_a_non_admitted_phase_without_a_checkpoint() {
        let conn = tables();
        assert!(phase(&conn, "model", "model", Some(1), None, Some(("r", 5))).is_err());
        assert!(phase(&conn, "tools", "tools", Some(1), None, None).is_err());
        assert!(
            phase(
                &conn,
                "admitted-checkpoint",
                "admitted",
                None,
                Some("{}"),
                None
            )
            .is_err()
        );
        phase(&conn, "admitted", "admitted", None, None, None).expect("an admitted phase");
        phase(&conn, "tools-ok", "tools", Some(1), Some("{}"), None).expect("a tools phase");
    }

    #[test]
    fn the_ddl_refuses_a_terminal_kind_its_cause_does_not_derive() {
        let conn = tables();
        let end = |run: &str, kind: &str, cause: &str| {
            conn.execute(
                "INSERT INTO session_runs (session_id, run, admission_json, terminal_kind,
                     terminal_cause_json, terminal_head_revision, terminal_at_ms)
                 VALUES ('s', ?1, NULL, ?2, ?3, NULL, 1)",
                params![run, kind, cause],
            )
        };
        let cancelled = r#"{"cause":"operator_cancelled","intent":1}"#;
        let refused = r#"{"cause":"refused","refusal":{"code":"x","message":"m"}}"#;
        assert!(end("a", "answered", cancelled).is_err());
        assert!(end("c", "answered", refused).is_err());
        assert!(end("d", "cancelled", r#"{"cause":"unknown"}"#).is_err());
        end("e", "cancelled", cancelled).expect("a cancelled cause ends cancelled");
        end("f", "failed", refused).expect("a refused cause fails");
        end("g", "answered", r#"{"cause":"commands_applied"}"#).expect("applied commands answer");
    }
}
