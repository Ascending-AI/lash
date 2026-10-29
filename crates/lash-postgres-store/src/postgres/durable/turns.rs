//! Turn phase state, the session commit and turn cancel requests on PostgreSQL: the `turns` domain's statements, apply and read
//! (I0, FIG-5194).
//!
//! Owned by V0 (FIG-5170), then L3 (FIG-5172). The dispatch in `durable/mod.rs` calls these inside the
//! fenced owner commit (or the mailbox commit), after the fence; a refusal
//! rolls the whole commit back. The DDL is in `schema.sql`.

use std::sync::LazyLock;

use lash_core_store::checkpoint_graph::NodeId;
use lash_core_store::store::HeadWriter;
use lash_core_store::store::RunAdmissionRecord;
use lash_core_store::store::RunTerminalCause;
use lash_core_store::store_backend_support::turn_cancel::turn_cancel_mode_from_wire;
use lash_core_store::store_backend_support::turn_cancel::turn_cancel_mode_wire;
use lash_core_store::store_backend_support::turn_cancel::turn_cancel_undelivered_from_wire;
use lash_core_store::store_backend_support::turn_cancel::turn_cancel_undelivered_wire;
use lash_durable::domain::namespace_value::NamespaceValue;
use lash_durable::domain::{
    DomainRefusal, ExecKey, OwnerKey, SessionCommitWrite, StoredValues, TurnCancelAnswer,
    TurnCancelRequest, TurnEnd, TurnNamespace, TurnRow, TurnWrite, UnfinishedPhase,
};
use lash_durable::{ActorKey, DurableError, DurableInstant, Epoch, Woken};
use lash_kernel_value::identity::SessionId;
use lash_store_sql::Dialect;
use lash_store_sql::durable::turns::TurnStatements;
use lash_vocabulary::identity::TurnId;
use sqlx::{PgConnection, Row};

use super::steps::{self, Collect, Entry, Shape, Step, Ty, Value};
use super::{Committing, corrupt, integer, node_ref, sqlx_failure};

static SQL: LazyLock<TurnStatements> =
    LazyLock::new(|| TurnStatements::render(Dialect::postgres()));

lash_store_sql::statements! {
    /// The turn statements only PostgreSQL issues: `UNNEST` records a run's
    /// namespace writes as one list.
    struct PostgresTurnStatements @ "durable_turn_postgres" {
        current_cancel = "SELECT c.turn_id, c.request_id, c.origin, c.reason, c.disposition, c.mode
            FROM session_runs r JOIN turn_phases p ON p.session_id = r.session_id AND p.run = r.run
            JOIN turn_cancel_requests c ON c.session_id = p.session_id AND c.turn_id = p.run
            WHERE r.session_id = ?1 AND r.admission_json IS NOT NULL AND r.terminal_kind IS NULL";

        /// A terminal's phase, namespaces and answered cancel share one end,
        /// answering the roots they held as one row: the PhaseRoot, the
        /// working execution, and the receipt with the namespace values
        /// (FIG-5949).
        delete_terminal_state = "WITH phase AS (
                 DELETE FROM turn_phases WHERE session_id = ?1 AND run = ?2
                 RETURNING checkpoint_ref, working_execution_ref, answer_ref
             ), namespaces AS (
                 DELETE FROM turn_namespaces WHERE session_id = ?1 AND run = ?2
                 RETURNING graph_root_ref
             ), cancel AS (
                 DELETE FROM turn_cancel_requests WHERE session_id = ?1 AND turn_id = ?2
             ), call_cancels AS (
                 DELETE FROM tool_call_cancel_requests WHERE session_id = ?1 AND run = ?2
             )
             SELECT (SELECT checkpoint_ref FROM phase) AS checkpoint_ref,
                 (SELECT working_execution_ref FROM phase) AS working_execution_ref,
                 ARRAY(SELECT answer_ref FROM phase WHERE answer_ref IS NOT NULL
                     UNION ALL
                     SELECT graph_root_ref FROM namespaces WHERE graph_root_ref IS NOT NULL)
                     AS closure_roots";

        /// Number revision `?2` of session `?1` as its next terminal, once,
        /// and advance the head's terminal ordinal to it.
        mark_terminal = "WITH marked AS (
                 UPDATE session_revisions SET terminal_ordinal = (
                     SELECT terminal_ordinal + 1 FROM session_head WHERE session_id = ?1
                 ) WHERE session_id = ?1 AND head_revision = ?2 AND terminal_ordinal IS NULL
                 RETURNING terminal_ordinal
             )
             UPDATE session_head SET terminal_ordinal = marked.terminal_ordinal
             FROM marked WHERE session_head.session_id = ?1";

        /// Whether session `?1`'s retention releases revisions at a commit
        /// (`until_gc` does not). No row when it recorded no policy.
        retention_releases = "SELECT retention_kind <> 'until_gc' AS releases
             FROM session_meta WHERE session_id = ?1";

        /// Record each plugin `?3[i]`'s namespace entry `?4[i]` for run `?2`
        /// of session `?1` with values root `?5[i]`, as the neutral
        /// `upsert_namespace` does one: `?6[i]` retains the held root.
        upsert_namespaces = "INSERT INTO turn_namespaces
                 (session_id, run, plugin, entry, graph_root_ref)
             SELECT ?1, ?2, written.plugin, written.entry, written.root
             FROM UNNEST(?3::text[], ?4::text[], ?5::bytea[], ?6::boolean[])
                 AS written(plugin, entry, root, held)
             ON CONFLICT (session_id, run, plugin)
             DO UPDATE SET entry = excluded.entry,
                 graph_root_ref = CASE
                     WHEN (SELECT written.held FROM UNNEST(?3::text[], ?6::boolean[])
                               AS written(plugin, held)
                           WHERE written.plugin = excluded.plugin)
                     THEN turn_namespaces.graph_root_ref
                     ELSE excluded.graph_root_ref END";

        /// The values root of each plugin `?3[i]`'s namespace row for run
        /// `?2` of session `?1`: what [`Self::upsert_namespaces`] may
        /// replace, and so retire (FIG-5949).
        namespace_roots = "SELECT graph_root_ref FROM turn_namespaces
             WHERE session_id = ?1 AND run = ?2 AND plugin = ANY(?3::text[])";
    }
}

static POSTGRES_SQL: LazyLock<PostgresTurnStatements> =
    LazyLock::new(|| PostgresTurnStatements::render(Dialect::postgres()));

fn refuse<T>(refusal: DomainRefusal) -> Result<T, DurableError> {
    Err(DurableError::Domain(refusal))
}

/// A store codec's refusal, as the durable port reports it.
fn encoding(error: &lash_core_store::store::StoreError) -> DurableError {
    DurableError::Store(lash_durable::StoreFailure {
        code: None,
        kind: lash_durable::StoreFailureKind::Corrupt,
        message: error.to_string(),
    })
}

/// The turn statements an owner commit runs as steps.
pub(super) static ADMIT_RUN: Entry = Entry::new(
    || &SQL.admit_run,
    &[Ty::Text, Ty::Text, Ty::Text],
    Shape::Returning,
);
pub(super) static INSERT_PHASE: Entry = Entry::new(
    || &SQL.insert_phase,
    &[
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Int,
        Ty::Int,
        Ty::Key,
        Ty::Text,
        Ty::Int,
        Ty::Int,
        Ty::Int,
    ],
    Shape::Exec,
);
pub(super) static ADVANCE_PHASE: Entry = Entry::new(
    || &SQL.advance_phase,
    &[
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Int,
        Ty::Int,
        Ty::Key,
        Ty::Text,
        Ty::Int,
        Ty::Int,
        Ty::Int,
        Ty::Text,
    ],
    Shape::Returning,
);
pub(super) static ADD_TOOL_CALLS: Entry = Entry::new(
    || &SQL.add_tool_calls,
    &[Ty::Text, Ty::Text, Ty::Int],
    Shape::Exec,
);
pub(super) static ANSWER_SET: Entry = Entry::new(
    || &SQL.answer_set,
    &[Ty::Text, Ty::Text, Ty::Key, Ty::Int],
    Shape::Returning,
);
pub(super) static OPEN_NAMED_RUN: Entry =
    Entry::new(|| &SQL.open_named_run, &[Ty::Text, Ty::Text], Shape::Select);
pub(super) static UPSERT_NAMESPACES: Entry = Entry::new(
    || &POSTGRES_SQL.upsert_namespaces,
    &[
        Ty::Text,
        Ty::Text,
        Ty::Texts,
        Ty::Texts,
        Ty::Keys,
        Ty::Bools,
    ],
    Shape::Exec,
);
pub(super) static WORKING_SET: Entry = Entry::new(
    || &SQL.working_set,
    &[Ty::Text, Ty::Text, Ty::Key, Ty::Key, Ty::Int],
    Shape::Returning,
);
/// The PhaseRoot an advance replaces and the receipt it consumes: retired
/// with the advance, and peeled unless the new phase names them again
/// (FIG-5949).
pub(super) static PHASE_ROOTS: Entry =
    Entry::new(|| &SQL.phase_roots, &[Ty::Text, Ty::Text], Shape::Select)
        .retiring("checkpoint_ref")
        .retiring("answer_ref");
/// The namespace values roots a namespace write may replace: retired with
/// it, and peeled unless a row still names them (FIG-5949).
pub(super) static NAMESPACE_ROOTS: Entry = Entry::new(
    || &POSTGRES_SQL.namespace_roots,
    &[Ty::Text, Ty::Text, Ty::Texts],
    Shape::Select,
)
.retiring("graph_root_ref");

/// A terminal's statements (FIG-5942): the run ends on the session's
/// published head, read by the statement; its revision is numbered and the
/// retention's release follows the session's own policy; the rows the run
/// still holds are settled; its execution's local edges end; and its
/// cells, phase and namespaces go, their roots retired.
pub(super) static END_RUN: Entry = Entry::new(
    || &SQL.end_run,
    &[
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Text,
        Ty::Int,
        Ty::Now,
        Ty::Key,
    ],
    Shape::Returning,
);
pub(super) static MARK_TERMINAL: Entry = Entry::new(
    || &POSTGRES_SQL.mark_terminal,
    &[Ty::Text, Ty::Int],
    Shape::Exec,
);
pub(super) static RETENTION: Entry = Entry::new(
    || &POSTGRES_SQL.retention_releases,
    &[Ty::Text],
    Shape::Select,
);
pub(super) static SETTLE_HELD_INPUTS: Entry = Entry::new(
    || &super::SQL.session_mail.settle_held_inputs,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Now],
    Shape::Exec,
);
pub(super) static SETTLE_HELD_BATCHES: Entry = Entry::new(
    || &super::SQL.session_mail.settle_held_batches,
    &[Ty::Text, Ty::Text, Ty::Now],
    Shape::Exec,
);
pub(super) static END_LOCAL: Entry = Entry::new(
    || &crate::artifact_store::artifact_sql().postgres.end_local,
    &[Ty::Text, Ty::Text, Ty::Text, Ty::Now, Ty::Texts, Ty::Text],
    Shape::EndLocal,
);
pub(super) static TERMINAL_STATE: Entry = Entry::new(
    || &POSTGRES_SQL.delete_terminal_state,
    &[Ty::Text, Ty::Text],
    Shape::Select,
)
.collecting(Collect::PhaseEnd("checkpoint_ref"))
.collecting(Collect::Closure("working_execution_ref"))
.collecting(Collect::Closures("closure_roots"));
/// A referrer's or an artifact's advisory lock, by its key.
pub(super) static LOCK_KEY: Entry = Entry::new(
    || &crate::connection_sql::connection_sql().lock_xact_by_text,
    &[Ty::Text],
    Shape::Select,
);

/// The capture slot of a terminal's retention read.
const TERMINAL_RETENTION: u8 = 2;

/// The end of `referrer`'s local edges, gated on `gate`, as a step.
pub(super) fn end_local_step(
    referrer: &lash_core_store::artifact_referrer::ArtifactReferrer,
    gate: Option<&lash_protocol_types::EffectJournalIdentity>,
) -> Result<Step, lash_core_store::store::StoreError> {
    let end = crate::artifact_store::LocalEndArguments::of(referrer, gate)?;
    Ok(Step::one(
        &END_LOCAL,
        vec![
            Value::text(end.kind),
            Value::text(end.id),
            Value::Text(end.gate),
            Value::Now,
            Value::texts(end.namespaces),
            Value::Text(end.gated_body),
        ],
    ))
}

/// The locks of the executions `writes` end, as steps, in key order:
/// taken before a head commit takes frame and artifact locks, as
/// publication takes them referrer first.
pub(super) fn lock_ending_executions(
    writes: &[lash_durable::DomainWrite],
) -> Result<Vec<Step>, lash_core_store::store::StoreError> {
    Ok(crate::obligation_ledger::ending_executions(writes)?
        .iter()
        .map(|referrer| {
            Step::any(
                &LOCK_KEY,
                vec![Value::text(crate::artifact_store::referrer_lock_key(
                    referrer,
                ))],
            )
        })
        .collect())
}

/// A terminal's program: run `run` of `session` ends with `cause` at head
/// revision `head_revision`.
fn terminal_steps(
    session: &SessionId,
    run: &TurnId,
    cause: &RunTerminalCause,
    head_revision: Option<u64>,
    transcript: Option<NodeId>,
) -> Result<Vec<Step>, DurableError> {
    let turn = || vec![Value::text(session.as_str()), Value::text(run.as_str())];
    let mut ended = turn();
    ended.extend([
        Value::text(cause.kind().as_str()),
        Value::text(cause.to_stored().map_err(|error| encoding(&error))?),
        Value::Int(head_revision.map(integer::<i64>).transpose()?),
        Value::Now,
        Value::Key(transcript.map(|transcript| transcript.to_hex())),
    ]);
    let mut program = vec![Step::one(&END_RUN, ended)];
    if let Some(revision) = head_revision {
        // The terminal is numbered once within its publishing transaction,
        // then the session's retention applies.
        program.push(Step::any(
            &MARK_TERMINAL,
            vec![
                Value::text(session.as_str()),
                Value::int(integer(revision)?),
            ],
        ));
        program.push(
            Step::any(&RETENTION, vec![Value::text(session.as_str())])
                .capturing(TERMINAL_RETENTION),
        );
        program.push(
            Step::any(
                &super::head_commit::PRUNE,
                vec![Value::int(0), Value::text(session.as_str())],
            )
            .when_flag(TERMINAL_RETENTION, "releases", true),
        );
        program.extend(
            super::retention::release_material_steps(session.as_str())
                .into_iter()
                .map(|step| step.when_flag(TERMINAL_RETENTION, "releases", true)),
        );
    }
    // What the run still holds as it ends is settled: a turn whose commit
    // settled its rows holds none, and a turn that ends with no commit (a
    // cancel) leaves its inputs and batches cancelled (ADR 0132 §11).
    let mut inputs = turn();
    inputs.extend([
        Value::text(lash_core_store::turn_input_vocabulary::TurnInputStateKind::Cancelled.as_str()),
        Value::Now,
    ]);
    program.push(Step::any(&SETTLE_HELD_INPUTS, inputs));
    let mut batches = turn();
    batches.push(Value::Now);
    program.push(Step::any(&SETTLE_HELD_BATCHES, batches));
    let journal = lash_protocol_types::ExecutionScope::turn(session.clone(), run.clone())
        .journal_identity()
        .map_err(|error| corrupt("turn journal", &error.to_string()))?;
    program.push(
        end_local_step(
            &lash_core_store::artifact_referrer::ArtifactReferrer::Execution(journal.clone()),
            Some(&journal),
        )
        .map_err(|error| encoding(&error))?,
    );
    // The cells the turn stopped in are never resumed, and the turn's
    // cancel request is answered by its terminal: neither has a reader
    // left. Nor has the turn's own ledger: its transcript names the
    // material its answered calls are read from.
    let cells = ExecKey::turn_cells(session, run);
    for entry in [
        &super::snapshots::CLEAR_UNDER,
        &super::graph::ADMISSIONS_UNDER,
        &super::run_records::DELETE_UNDER,
    ] {
        program.push(Step::any(entry, vec![Value::text(cells.as_str())]));
    }
    program.push(Step::any(
        &super::run_records::DELETE_OWNER,
        vec![Value::text(
            OwnerKey::Turn(session.clone(), run.clone()).stored(),
        )],
    ));
    // The turn's phase row and namespaces go with it, and their roots with
    // them: its working execution, receipt and namespace values whole, its
    // PhaseRoot alone (the TurnContent ordering constraint,
    // [`super::Retired`]).
    program.push(Step::any(&TERMINAL_STATE, turn()));
    Ok(program)
}

/// `write`'s step program at `epoch`: each write is guarded on its turn,
/// the admission on the session having none open. A cancelled terminal has
/// none: its stored cause names the inputs its stop settled, which are
/// read.
pub(super) fn steps(epoch: Epoch, write: &TurnWrite) -> Result<Option<Vec<Step>>, DurableError> {
    let turn = |session: &SessionId, run: &TurnId| {
        vec![Value::text(session.as_str()), Value::text(run.as_str())]
    };
    Ok(Some(match write {
        TurnWrite::Admit {
            session,
            run,
            admission,
            turn_deadline,
        } => {
            let (phase, argument) = UnfinishedPhase::Admitted.stored();
            let mut admitted = turn(session, run);
            admitted.push(Value::text(
                admission.to_stored().map_err(|error| encoding(&error))?,
            ));
            let mut phased = turn(session, run);
            phased.extend([
                Value::text(phase),
                Value::Int(argument.map(integer::<i64>).transpose()?),
                Value::int(0),
                Value::Key(None),
                Value::Text(None),
                Value::Int(None),
                Value::Int(turn_deadline.map(|deadline| deadline.0)),
                Value::int(epoch.0),
            ]);
            vec![
                Step::one(&ADMIT_RUN, admitted),
                Step::any(&INSERT_PHASE, phased),
            ]
        }
        TurnWrite::Advance {
            session,
            run,
            phase,
            iteration,
        } => {
            let (stored, argument) = phase.stored();
            let pin = phase.model();
            let mut values = turn(session, run);
            values.extend([
                Value::text(stored),
                Value::Int(argument.map(integer::<i64>).transpose()?),
                Value::int(i64::from(*iteration)),
                Value::key(phase.checkpoint().to_hex()),
                Value::Text(pin.map(|pin| pin.request_ref.clone())),
                Value::Int(pin.map(|pin| pin.deadline.0)),
                Value::int(epoch.0),
                Value::Int(pin.map(|pin| i64::from(pin.call))),
                Value::Text(pin.map(|pin| pin.stream_from.clone())),
            ]);
            vec![
                Step::any(&PHASE_ROOTS, turn(session, run)),
                Step::one(&ADVANCE_PHASE, values),
            ]
        }
        TurnWrite::AddToolCalls {
            session,
            run,
            calls,
        } => {
            let mut values = turn(session, run);
            values.push(Value::int(integer::<i64>(*calls)?));
            vec![Step::any(&ADD_TOOL_CALLS, values)]
        }
        TurnWrite::Answer {
            session,
            run,
            answer,
        } => {
            let mut values = turn(session, run);
            values.extend([Value::key(answer.to_hex()), Value::int(epoch.0)]);
            vec![Step::one(&ANSWER_SET, values)]
        }
        TurnWrite::Namespaces {
            session,
            run,
            namespaces,
        } => {
            // One write per plugin: a later write of a plugin replaces an
            // earlier one's entry, and its values unless it holds them.
            let mut written: Vec<(&str, String, Option<String>, bool)> = Vec::new();
            for namespace in namespaces {
                let entry = serde_json::to_string(&namespace.entry)
                    .map_err(|error| corrupt("turn namespace entry", &error.to_string()))?;
                let index = written
                    .iter()
                    .position(|(plugin, ..)| *plugin == namespace.plugin.as_str());
                let earlier = index.map(|index| written.remove(index));
                let (root, held) = match (&namespace.values, earlier) {
                    (StoredValues::Root(root), _) => (Some(root.to_hex()), false),
                    (StoredValues::Base, _) => (None, false),
                    (StoredValues::Held, Some((_, _, root, held))) => (root, held),
                    (StoredValues::Held, None) => (None, true),
                };
                written.push((namespace.plugin.as_str(), entry, root, held));
            }
            let mut program = vec![Step::one(&OPEN_NAMED_RUN, turn(session, run))];
            if !written.is_empty() {
                let mut values = turn(session, run);
                values.extend([
                    Value::Texts(
                        written
                            .iter()
                            .map(|(plugin, ..)| Some((*plugin).to_owned()))
                            .collect(),
                    ),
                    Value::Texts(
                        written
                            .iter()
                            .map(|(_, entry, ..)| Some(entry.clone()))
                            .collect(),
                    ),
                    Value::Keys(written.iter().map(|(_, _, root, _)| root.clone()).collect()),
                    Value::Bools(written.iter().map(|(.., held)| *held).collect()),
                ]);
                program.push(Step::any(
                    &NAMESPACE_ROOTS,
                    [
                        turn(session, run),
                        vec![Value::Texts(
                            written
                                .iter()
                                .map(|(plugin, ..)| Some((*plugin).to_owned()))
                                .collect(),
                        )],
                    ]
                    .concat(),
                ));
                program.push(Step::any(&UPSERT_NAMESPACES, values));
            }
            program
        }
        TurnWrite::Working {
            session,
            run,
            expected,
            working,
        } => {
            let mut values = turn(session, run);
            values.extend([
                Value::Key(working.map(|working| working.to_hex())),
                Value::Key(expected.map(|expected| expected.to_hex())),
                Value::int(epoch.0),
            ]);
            vec![
                Step::one(&WORKING_SET, values)
                    .retiring(expected.map(|expected| expected.to_hex())),
            ]
        }
        TurnWrite::Terminal {
            cause,
            session,
            run,
            head_revision,
            transcript,
        } => {
            if matches!(cause.as_ref(), RunTerminalCause::Cancelled { .. }) {
                return Ok(None);
            }
            terminal_steps(session, run, cause, *head_revision, *transcript)?
        }
    }))
}

pub(super) async fn apply(
    tx: &mut PgConnection,
    commit: &Committing<'_>,
    write: &TurnWrite,
) -> Result<(), DurableError> {
    match write {
        TurnWrite::AddToolCalls { .. } => {
            let program = steps(commit.epoch, write)?.unwrap_or_default();
            steps::run(tx, &program, &commit.steps()).await?;
            Ok(())
        }
        TurnWrite::Admit { session, .. } => {
            let program = steps(commit.epoch, write)?.unwrap_or_default();
            match steps::run(tx, &program, &commit.steps()).await? {
                None => Ok(()),
                Some(_) => refuse(DomainRefusal::OpenTurnExists {
                    session: session.clone(),
                }),
            }
        }
        TurnWrite::Advance { session, run, .. }
        | TurnWrite::Answer { session, run, .. }
        | TurnWrite::Namespaces { session, run, .. }
        | TurnWrite::Working { session, run, .. } => {
            let program = steps(commit.epoch, write)?.unwrap_or_default();
            match steps::run(tx, &program, &commit.steps()).await? {
                None => Ok(()),
                Some(_) => refuse(DomainRefusal::TurnNotOpen {
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
            let mut cause = cause.as_ref().clone();
            if let RunTerminalCause::Cancelled {
                evidence,
                input_outcome,
            } = &mut cause
            {
                *input_outcome = super::session_mail::cancel_inputs(
                    &mut *tx,
                    &commit.steps(),
                    session,
                    run,
                    evidence.undelivered,
                )
                .await?;
            }
            let program = terminal_steps(session, run, &cause, *head_revision, *transcript)?;
            match steps::run(tx, &program, &commit.steps()).await? {
                None => {
                    super::packages::end_actor_executions(tx, commit.actor).await?;
                    Ok(())
                }
                Some(refused) if std::ptr::eq(program[refused.index].entry, &END_RUN) => {
                    refuse(DomainRefusal::TurnNotOpen {
                        session: session.clone(),
                        run: run.clone(),
                    })
                }
                Some(refused) => Err(corrupt(
                    "terminal step",
                    program[refused.index].entry.name(),
                )),
            }
        }
    }
}

pub(super) async fn apply_session_commit(
    tx: &mut super::Tx,
    commit: &Committing<'_>,
    write: &SessionCommitWrite,
) -> Result<lash_core_store::store::RuntimeCommitReceipt, DurableError> {
    let refused = |error: &lash_core_store::store::StoreError| {
        DurableError::session_commit(write.session.clone(), error)
    };
    let runtime_commit = lash_core_store::store::decode_session_commit(&write.commit_json)
        .map_err(|error| refused(&error))?;
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
    let planner = lash_core_store::store::RuntimeCommitPlanner::prepare(runtime_commit, tx.fleet())
        .map_err(|error| refused(&error))?;
    let writer = head_writer(commit.actor, &write.session);
    super::head_commit::apply(
        tx,
        &planner,
        writer,
        &commit.steps(),
        commit.checkpoint_ref_chunk,
        commit.configs,
    )
    .await
    .map_err(|error| refused(&error))
}

/// The commit `write` carries, prepared under `fleet`: `None` when it does
/// not decode or names another session or head than its write, which the
/// explicit form refuses.
pub(super) fn prepared(
    write: &SessionCommitWrite,
    fleet: lash_core_store::store::FleetFormat,
) -> Option<lash_core_store::store::RuntimeCommitPlanner> {
    let commit = lash_core_store::store::decode_session_commit(&write.commit_json).ok()?;
    if commit.session_id != write.session || commit.expected_head_revision != write.expected_head {
        return None;
    }
    lash_core_store::store::RuntimeCommitPlanner::prepare(commit, fleet).ok()
}

/// Who writes a session head commit fenced by `actor`: the session's own
/// actor, whose epoch fences every head write it makes (FIG-5355), or any
/// other owner, which the head's ownership gate checks as a store writer.
pub(super) fn head_writer(actor: &ActorKey, session: &SessionId) -> HeadWriter {
    if ActorKey::session(session.as_str()).is_ok_and(|own| &own == actor) {
        HeadWriter::SessionActor
    } else {
        HeadWriter::Store
    }
}

pub(super) async fn request_cancel(
    tx: &mut PgConnection,
    request: &TurnCancelRequest,
    now: DurableInstant,
) -> Result<(TurnCancelAnswer, Option<Woken>), DurableError> {
    let session = request.session.as_str();
    let run = request.run.as_str();
    let actor = ActorKey::session(session).map_err(|_| corrupt("session id", session))?;
    // A turn no run opened yet is its queued input: the withdraw takes the
    // open row first, and an admission that bound it first leaves the open
    // run this request cancels, which the read below then sees (FIG-5262).
    if let Some(input) =
        super::session_mail::withdraw_queued(tx, &request.session, &request.run, now.0).await?
    {
        let (woken, _) = super::wake_within(tx, &actor, false, now).await?;
        return Ok((TurnCancelAnswer::Withdrawn { input }, Some(woken)));
    }
    let open: Option<String> = crate::observed_sql::query_scalar(SQL.open_named_run.sql())
        .bind(session)
        .bind(run)
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    if open.is_none() {
        return Ok((TurnCancelAnswer::AlreadyEnded, None));
    }
    let answer = match cancel_of(tx, &request.session, &request.run).await? {
        None => {
            crate::observed_sql::query(SQL.insert_cancel.sql())
                .bind(session)
                .bind(run)
                .bind(&request.request_id)
                .bind(&request.origin)
                .bind(&request.reason)
                .bind(turn_cancel_undelivered_wire(request.undelivered))
                .bind(turn_cancel_mode_wire(request.mode))
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            TurnCancelAnswer::Requested
        }
        Some(mut accepted) if request.escalates(&accepted) => {
            crate::observed_sql::query(SQL.escalate_cancel.sql())
                .bind(session)
                .bind(run)
                .bind(&request.request_id)
                .bind(&request.origin)
                .bind(&request.reason)
                .bind(turn_cancel_mode_wire(request.mode))
                .execute(crate::observed_sql::executor(&mut *tx))
                .await
                .map_err(sqlx_failure)?;
            accepted.escalate_to(request);
            TurnCancelAnswer::Escalated { accepted }
        }
        Some(accepted) if accepted.undelivered != request.undelivered => {
            return Ok((TurnCancelAnswer::PolicyConflict { accepted }, None));
        }
        Some(accepted) => return Ok((TurnCancelAnswer::AlreadyRequested { accepted }, None)),
    };
    let (woken, _) = super::wake_within(tx, &actor, true, now).await?;
    Ok((answer, Some(woken)))
}

/// Current externally accepted cancel, read under the resolution owner fence.
pub(super) async fn current_cancel(
    tx: &mut PgConnection,
    actor: &ActorKey,
) -> Result<Option<TurnCancelRequest>, DurableError> {
    if actor.kind() != lash_durable::ActorKind::Session {
        return Ok(None);
    }
    let session = SessionId::try_from(actor.id().to_owned())
        .map_err(|_| corrupt("session id", actor.id()))?;
    type CurrentCancel = (
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        String,
    );
    let row: Option<CurrentCancel> =
        crate::observed_sql::query_as(POSTGRES_SQL.current_cancel.sql())
            .bind(session.as_str())
            .fetch_optional(crate::observed_sql::executor(&mut *tx))
            .await
            .map_err(sqlx_failure)?;
    row.map(|(run, request, origin, reason, disposition, mode)| {
        let run = TurnId::try_from(run).map_err(|error| corrupt("turn id", &error.to_string()))?;
        decode_cancel(&session, &run, (request, origin, reason, disposition, mode))
    })
    .transpose()
}

/// A stored cancel request: request id, origin, reason, disposition, mode.
pub(super) type StoredCancel = (String, Option<String>, Option<String>, String, String);

/// The cancel request run `run` of `session` accepted.
async fn cancel_of(
    tx: &mut PgConnection,
    session: &SessionId,
    run: &TurnId,
) -> Result<Option<TurnCancelRequest>, DurableError> {
    let stored: Option<StoredCancel> = crate::observed_sql::query_as(SQL.cancel_of.sql())
        .bind(session.as_str())
        .bind(run.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    stored
        .map(|stored| decode_cancel(session, run, stored))
        .transpose()
}

/// Run `run` of session `session`'s accepted cancel request, as
/// `turn_cancel_requests` stores it.
pub(super) fn decode_cancel(
    session: &SessionId,
    run: &TurnId,
    (request_id, origin, reason, disposition, mode): StoredCancel,
) -> Result<TurnCancelRequest, DurableError> {
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

pub(super) async fn turn_end(
    tx: &mut PgConnection,
    session: &SessionId,
    run: &TurnId,
) -> Result<Option<TurnEnd>, DurableError> {
    let Some(row) = crate::observed_sql::query(SQL.ended.sql())
        .bind(session.as_str())
        .bind(run.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let cause: String = row.try_get(0).map_err(sqlx_failure)?;
    let head_revision: Option<i64> = row.try_get(1).map_err(sqlx_failure)?;
    let cause = RunTerminalCause::from_stored(&cause).map_err(|error| encoding(&error))?;
    let head_revision = head_revision.map(integer::<u64>).transpose()?;
    Ok(Some(TurnEnd {
        cause,
        head_revision,
    }))
}

pub(super) async fn turn_namespaces(
    tx: &mut PgConnection,
    session: &SessionId,
    run: &TurnId,
    limits: lash_core_store::checkpoint_graph::DecodeLimits,
) -> Result<Vec<TurnNamespace>, DurableError> {
    let rows = crate::observed_sql::query(SQL.namespaces.sql())
        .bind(session.as_str())
        .bind(run.as_str())
        .fetch_all(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?;
    let mut namespaces = Vec::with_capacity(rows.len());
    for row in rows {
        let plugin: String = row.try_get(0).map_err(sqlx_failure)?;
        let entry: String = row.try_get(1).map_err(sqlx_failure)?;
        let root: Option<String> = crate::keys::get(&row, 2).map_err(sqlx_failure)?;
        let entry =
            serde_json::from_str(&entry).map_err(|_| corrupt("turn namespace entry", &plugin))?;
        let body = match root {
            None => None,
            Some(root) => {
                let root = node_ref(&root)?;
                let nodes = super::graph::graph_closure(tx, root, &limits).await?;
                Some(NamespaceValue::load(&nodes, root, &plugin, limits)?.body)
            }
        };
        namespaces.push(TurnNamespace {
            plugin,
            entry,
            body,
        });
    }
    Ok(namespaces)
}

pub(super) async fn turn(
    tx: &mut PgConnection,
    session: &SessionId,
) -> Result<Option<TurnRow>, DurableError> {
    let Some(row) = crate::observed_sql::query(SQL.unfinished.sql())
        .bind(session.as_str())
        .fetch_optional(crate::observed_sql::executor(&mut *tx))
        .await
        .map_err(sqlx_failure)?
    else {
        return Ok(None);
    };
    let decode = |error: sqlx::Error| sqlx_failure(error);
    let run: String = row.try_get(0).map_err(decode)?;
    let run = TurnId::try_from(run.clone()).map_err(|_| corrupt("turn id", &run))?;
    let stored_phase: String = row.try_get(2).map_err(decode)?;
    let argument: Option<i64> = row.try_get(3).map_err(decode)?;
    let argument = argument.map(integer::<u64>).transpose()?;
    let checkpoint: Option<String> = crate::keys::get(&row, 5).map_err(decode)?;
    let checkpoint = checkpoint.as_deref().map(node_ref).transpose()?;
    let working: Option<String> = crate::keys::get(&row, 12).map_err(decode)?;
    let working = working.as_deref().map(node_ref).transpose()?;
    let answer: Option<String> = crate::keys::get(&row, 13).map_err(decode)?;
    let answer = answer.as_deref().map(node_ref).transpose()?;
    let request: Option<String> = row.try_get(6).map_err(decode)?;
    let deadline: Option<i64> = row.try_get(7).map_err(decode)?;
    let stream_from: Option<String> = row.try_get(11).map_err(decode)?;
    let pin = request
        .zip(deadline.map(DurableInstant))
        .zip(stream_from)
        .map(|((request, deadline), stream_from)| (request, deadline, stream_from));
    let model_calls: i64 = row.try_get(10).map_err(decode)?;
    let model_calls = integer::<u32>(model_calls)?;
    let phase = UnfinishedPhase::parse(
        &stored_phase,
        argument,
        checkpoint,
        pin,
        model_calls,
        answer,
    )
    .ok_or_else(|| corrupt("turn phase", &stored_phase))?;
    let iteration: i64 = row.try_get(4).map_err(decode)?;
    let iteration = integer::<u32>(iteration)?;
    let turn_deadline: Option<i64> = row.try_get(8).map_err(decode)?;
    let written_epoch = Epoch(row.try_get(9).map_err(decode)?);
    let admission: String = row.try_get(1).map_err(decode)?;
    let admission =
        RunAdmissionRecord::from_stored(&admission).map_err(|error| encoding(&error))?;
    let cancel = cancel_of(tx, session, &run).await?;
    Ok(Some(TurnRow {
        session: session.clone(),
        run,
        admission,
        phase,
        working,
        iteration,
        model_calls,
        tool_calls: integer::<u64>(row.try_get::<i64, _>(14).map_err(decode)?)?,
        turn_deadline: turn_deadline.map(DurableInstant),
        written_epoch,
        cancel,
    }))
}

/// The storage laws of a turn's stored forms (FIG-5221) on PostgreSQL: the
/// DDL itself refuses a phase row without what its phase restores from, and
/// a run terminal whose kind its cause does not derive.
#[cfg(test)]
mod ddl_tests {
    use sqlx::Connection as _;
    const CHECKPOINT: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    use crate::testing::IsolatedDatabase;

    async fn connection() -> Option<(IsolatedDatabase, sqlx::PgConnection)> {
        let Some(database_url) = crate::postgres_test_support::database_url() else {
            eprintln!("skipping the DDL laws: database URL is not set");
            return None;
        };
        let database = IsolatedDatabase::create(&database_url).await;
        crate::testing::connect(database.url())
            .await
            .expect("provision the isolated store");
        let mut connection = sqlx::PgConnection::connect(database.url())
            .await
            .expect("connect to the isolated store");
        crate::observed_sql::query(
            "INSERT INTO lash_blobs (hash, content, encoding) VALUES (decode(repeat('00', 32), 'hex'), '\\x00', 1)",
        )
        .execute(&mut connection)
        .await
        .expect("a checkpoint blob");
        Some((database, connection))
    }

    /// Whether `result` is a CHECK constraint's refusal.
    fn refused<T: std::fmt::Debug>(result: Result<T, sqlx::Error>) -> bool {
        match result {
            Err(error) => error
                .as_database_error()
                .and_then(|error| error.code())
                .is_some_and(|code| code == "23514"),
            Ok(_) => false,
        }
    }

    async fn phase(
        connection: &mut sqlx::PgConnection,
        run: &str,
        phase: &str,
        argument: Option<i64>,
        checkpoint: Option<&str>,
        pin: Option<(&str, i64)>,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        crate::observed_sql::query(
            "INSERT INTO lash_turn_phases (session_id, run, phase, phase_arg, iteration,
                 checkpoint_ref, model_request_ref, model_deadline_ms, model_stream_from,
                 written_epoch)
             VALUES ('s', $1, $2, $3, 0, $4, $5, $6, $7, 1)",
        )
        .bind(run)
        .bind(phase)
        .bind(argument)
        .bind(checkpoint.map(crate::keys::key))
        .bind(pin.map(|(request, _)| request))
        .bind(pin.map(|(_, deadline)| deadline))
        .bind(pin.map(|_| "stream-cursor"))
        .execute(crate::observed_sql::executor(connection))
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_ddl_refuses_a_model_phase_without_its_pin() {
        let Some((_database, mut conn)) = connection().await else {
            return;
        };
        assert!(refused(
            phase(
                &mut conn,
                "unpinned",
                "model",
                Some(1),
                Some(CHECKPOINT),
                None
            )
            .await
        ));
        assert!(refused(
            phase(
                &mut conn,
                "tools-pinned",
                "tools",
                Some(1),
                Some(CHECKPOINT),
                Some(("r", 5))
            )
            .await
        ));
        phase(
            &mut conn,
            "pinned",
            "model",
            Some(1),
            Some(CHECKPOINT),
            Some(("r", 5)),
        )
        .await
        .expect("a pinned model phase is stored");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_ddl_refuses_a_non_admitted_phase_without_a_checkpoint() {
        let Some((_database, mut conn)) = connection().await else {
            return;
        };
        assert!(refused(
            phase(&mut conn, "model", "model", Some(1), None, Some(("r", 5))).await
        ));
        assert!(refused(
            phase(&mut conn, "tools", "tools", Some(1), None, None).await
        ));
        assert!(refused(
            phase(
                &mut conn,
                "admitted-checkpoint",
                "admitted",
                None,
                Some(CHECKPOINT),
                None
            )
            .await
        ));
        phase(&mut conn, "admitted", "admitted", None, None, None)
            .await
            .expect("an admitted phase");
        phase(
            &mut conn,
            "tools-ok",
            "tools",
            Some(1),
            Some(CHECKPOINT),
            None,
        )
        .await
        .expect("a tools phase");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_ddl_refuses_a_terminal_kind_its_cause_does_not_derive() {
        let Some((_database, mut conn)) = connection().await else {
            return;
        };
        let mut end = async |run: &str, kind: &str, cause: &str| {
            crate::observed_sql::query(
                "INSERT INTO lash_session_runs (session_id, run, admission_json, terminal_kind,
                     terminal_cause_json, terminal_head_revision, terminal_at_ms)
                 VALUES ('s', $1, NULL, $2, $3, NULL, 1)",
            )
            .bind(run)
            .bind(kind)
            .bind(cause)
            .execute(crate::observed_sql::executor(&mut conn))
            .await
        };
        let cancelled = r#"{"cause":"operator_cancelled","intent":1}"#;
        let refused_cause = r#"{"cause":"refused","refusal":{"code":"x","message":"m"}}"#;
        assert!(refused(end("a", "answered", cancelled).await));
        assert!(refused(end("c", "answered", refused_cause).await));
        assert!(refused(
            end("d", "cancelled", r#"{"cause":"unknown"}"#).await
        ));
        end("e", "cancelled", cancelled)
            .await
            .expect("a cancelled cause ends cancelled");
        end("f", "failed", refused_cause)
            .await
            .expect("a refused cause fails");
        end("g", "answered", r#"{"cause":"commands_applied"}"#)
            .await
            .expect("applied commands answer");
    }
}
