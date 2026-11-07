//! The session's mailbox on SQLite: the `session_mail` domain's read and
//! apply (L3s, FIG-5196).
//!
//! The dispatch in `durable/mod.rs` calls [`apply`] inside the fenced owner
//! commit, after the fence; a refusal rolls the whole commit back.

use crate::Prepare as _;
use lash_durable::domain::session_mail::{batch_kind, decode_batch, decode_input};
use lash_durable::domain::{
    DomainRefusal, MailBatchKind, OperationCancelAnswer, OperationCancelRequest, SessionMailWrite,
    SessionMailbox, queued_input_run,
};
use lash_durable::{ActorKey, DurableInstant, Woken};
use lash_durable::{DurableError, StoreFailure, StoreFailureKind};
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::BatchId;
use lash_vocabulary::identity::InputId;
use lash_vocabulary::identity::TurnId;
use rusqlite::{Connection, OptionalExtension};

use super::{Answer, Committing, SQL};

lash_store_sql::statements! {
    struct SqliteSessionMailStatements @ "durable_session_mail_sqlite" {
        /// Standing, bound run and all unbound mail share one SQL snapshot.
        mailbox = "SELECT
            (SELECT count(*) FROM session_meta WHERE session_id = ?1),
            (SELECT count(*) FROM deleted_sessions WHERE session_id = ?1),
            (SELECT count(*) FROM session_meta WHERE session_id = ?1 AND closing_intent IS NOT NULL),
            (SELECT admitted_run FROM pending_turn_inputs
                WHERE session_id = ?1 AND admitted_run IS NOT NULL
                  AND {{nonterminal_turn_input_state(state)}}
             UNION ALL SELECT admitted_run FROM queued_work_batches
                WHERE session_id = ?1 AND admitted_run IS NOT NULL AND terminal_cause IS NULL LIMIT 1) AS bound_run,
            (SELECT json_group_array(json_array(input_id, enqueue_seq, source_key, run_spec_hash,
                state, ingress_json, trace_cause_json, origin_json)) FROM (
                 SELECT input_id, enqueue_seq, source_key, run_spec_hash, state, ingress_json,
                     trace_cause_json, origin_json FROM pending_turn_inputs
                 WHERE session_id = ?1 AND admitted_run IS NULL
                   AND {{undelivered_turn_input_state(state)}} ORDER BY enqueue_seq)) AS inputs,
            (SELECT json_group_array(json_array(batch_id, enqueue_seq, delivery_policy, payload_json,
                CASE WHEN cancel_request_json IS NULL THEN 0 ELSE 1 END)) FROM (
                 SELECT batch_id, enqueue_seq, delivery_policy, payload_json, cancel_request_json
                 FROM queued_work_batches WHERE session_id = ?1 AND admitted_run IS NULL
                   AND terminal_cause IS NULL ORDER BY enqueue_seq))";
        bind_inputs = "UPDATE pending_turn_inputs SET admitted_run = ?3, admitted_by = ?3
            WHERE session_id = ?1 AND input_id IN (SELECT value FROM json_each(?2))
              AND (admitted_run IS NULL OR admitted_run = ?3)
              AND {{undelivered_turn_input_state(state)}} RETURNING input_id";
        bind_batches = "UPDATE queued_work_batches SET admitted_run = ?3, admitted_by = ?3
            WHERE session_id = ?1 AND batch_id IN (SELECT value FROM json_each(?2))
              AND (admitted_run IS NULL OR admitted_run = ?3)
              AND terminal_cause IS NULL RETURNING batch_id";
    }
}

static SQLITE: std::sync::LazyLock<SqliteSessionMailStatements> = std::sync::LazyLock::new(|| {
    SqliteSessionMailStatements::render(
        crate::schema_layout::MAIN.with_vocabulary(crate::turn_ingress::TURN_INPUT_LIFECYCLE),
    )
});

/// One snapshot of the operation's command and unfinished-run authorities.
pub(super) fn operation_is_open(
    tx: &Connection,
    operation: &lash_durable::domain::OperationRun,
) -> Answer<bool> {
    let run = operation.run_id();
    Ok(Ok(tx
        .statement(SQL.session_mail.operation_is_open.sql())?
        .query_row(
            rusqlite::params![
                operation.session_id.as_str(),
                operation.operation_id,
                run.as_str()
            ],
            |row| row.get(0),
        )?))
}

fn undecodable(what: &str, detail: impl std::fmt::Display) -> DurableError {
    DurableError::Store(StoreFailure {
        code: None,
        kind: StoreFailureKind::Corrupt,
        message: format!("session mail {what} does not decode: {detail}"),
    })
}

fn count(value: i64) -> bool {
    value > 0
}

/// Session `session`'s standing: `(live, closing)`.
fn standing(tx: &Connection, session: &SessionId) -> rusqlite::Result<(bool, bool)> {
    let (meta, deleted, closing) =
        tx.statement(SQL.session_mail.standing.sql())?
            .query_row([session.as_str()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
    Ok((count(meta) && !count(deleted), count(closing)))
}

/// The session's open mail, read in one snapshot.
pub(super) fn read(tx: &Connection, session: &SessionId) -> Answer<SessionMailbox> {
    let (meta, deleted, closing, bound, inputs, batches) = tx
        .statement(SQLITE.mailbox.sql())?
        .query_row([session.as_str()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
    let mut mailbox = SessionMailbox {
        live: count(meta) && !count(deleted),
        closing: count(closing),
        ..SessionMailbox::default()
    };
    if !mailbox.live {
        return Ok(Ok(mailbox));
    }
    mailbox.bound_run = match bound.map(TurnId::parse).transpose() {
        Ok(run) => run,
        Err(error) => return Ok(Err(undecodable("bound run", error))),
    };
    type Input = (
        String,
        i64,
        Option<String>,
        Option<String>,
        String,
        String,
        Option<String>,
        String,
    );
    let inputs: Vec<Input> = match serde_json::from_str(&inputs) {
        Ok(inputs) => inputs,
        Err(error) => return Ok(Err(undecodable("input rows", error))),
    };
    for (input, seq, source_key, run_spec_hash, state, ingress_json, trace_cause, origin_json) in
        inputs
    {
        match decode_input(
            input,
            seq,
            source_key,
            run_spec_hash,
            (&state, &ingress_json),
            trace_cause.as_deref(),
            &origin_json,
        ) {
            Ok(input) => mailbox.inputs.push(input),
            Err(error) => return Ok(Err(error)),
        }
    }
    let batches: Vec<(String, i64, String, String, i64)> = match serde_json::from_str(&batches) {
        Ok(batches) => batches,
        Err(error) => return Ok(Err(undecodable("batch rows", error))),
    };
    for (batch, seq, policy, payload_json, cancel) in batches {
        match decode_batch(batch, seq, &policy, &payload_json, count(cancel)) {
            Ok(batch) => mailbox.batches.push(batch),
            Err(error) => return Ok(Err(error)),
        }
    }
    Ok(Ok(mailbox))
}

/// A host's cancel of plugin task `request.batch`'s operation run
/// (FIG-5343), in one mail transaction: a command no operation run entered
/// is withdrawn, with a wake; an entered one records the first request,
/// with a control wake, so its running handler is cancelled.
pub(super) fn request_operation_cancel(
    tx: &Connection,
    request: &OperationCancelRequest,
    now: DurableInstant,
) -> Answer<(OperationCancelAnswer, Option<Woken>)> {
    let session = request.session.as_str();
    let batch = request.batch.as_str();
    let actor = match ActorKey::session(session) {
        Ok(actor) => actor,
        Err(_) => return Ok(Err(undecodable("session id", session))),
    };
    let open = tx
        .statement(SQL.session_mail.open_operation.sql())?
        .query_row([session, batch], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .optional()?;
    let Some((payload_json, accepted)) = open else {
        return Ok(Ok((OperationCancelAnswer::NotOpen, None)));
    };
    match batch_kind(&payload_json) {
        Ok(MailBatchKind::Operation) => {}
        Ok(MailBatchKind::Control) => return Ok(Ok((OperationCancelAnswer::NotOpen, None))),
        Err(error) => return Ok(Err(error)),
    }
    if let Some(request_json) = accepted {
        return Ok(Ok((
            OperationCancelAnswer::AlreadyRequested { request_json },
            None,
        )));
    }
    let withdrawn = tx
        .statement(SQL.session_mail.withdraw_unentered.sql())?
        .query_row(rusqlite::params![session, batch, now.0], |_| Ok(()))
        .optional()?;
    if withdrawn.is_some() {
        return Ok(super::wake_within(tx, &actor, false, now)?
            .map(|(woken, _)| (OperationCancelAnswer::Withdrawn, Some(woken))));
    }
    tx.statement(SQL.session_mail.request_operation_cancel.sql())?
        .query_row(
            rusqlite::params![session, batch, request.request_json],
            |_| Ok(()),
        )?;
    Ok(super::wake_within(tx, &actor, true, now)?
        .map(|(woken, _)| (OperationCancelAnswer::Requested, Some(woken))))
}

/// Settle what run `run` of `session` still holds as it ends: a turn whose
/// commit settled its rows holds none, and a turn that ends with no commit
/// (a cancel) leaves its inputs `cancelled` and its batches cancelled, so no
/// row stays bound to an ended run (ADR 0132 §11).
pub(super) fn settle_held(
    tx: &Connection,
    session: &SessionId,
    run: &TurnId,
    now_ms: i64,
) -> rusqlite::Result<()> {
    tx.statement(SQL.session_mail.settle_held_inputs.sql())?
        .execute(rusqlite::params![
            session.as_str(),
            run.as_str(),
            lash_core_store::turn_input_vocabulary::TurnInputStateKind::Cancelled.as_str(),
            now_ms,
        ])?;
    tx.statement(SQL.session_mail.settle_held_batches.sql())?
        .execute(rusqlite::params![session.as_str(), run.as_str(), now_ms])?;
    Ok(())
}

/// Record the inputs this stop affects at the terminal transaction's
/// boundary, after its head commit settled delivered inputs. A drop binds
/// them for `settle_held`; deferral keeps their original ingress position.
pub(super) fn cancel_inputs(
    tx: &Connection,
    session: &SessionId,
    run: &TurnId,
    disposition: lash_protocol_types::session_model::TurnCancelUndeliveredInputPolicy,
) -> Answer<lash_core_store::turn_control_vocabulary::TurnCancelInputOutcome> {
    let rows = tx
        .statement(SQL.session_mail.cancel_inputs.sql())?
        .query_map([session.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut outcome = lash_core_store::turn_control_vocabulary::TurnCancelInputOutcome::default();
    for (input, payload, ingress) in rows {
        let affected = match lash_core_store::store_backend_support::turn_cancel::affected_input(
            run,
            disposition,
            input,
            &payload,
            &ingress,
        ) {
            Ok(Some(affected)) => affected,
            Ok(None) => continue,
            Err(error) => return Ok(Err(undecodable("cancelled input", error))),
        };
        if disposition == lash_protocol_types::session_model::TurnCancelUndeliveredInputPolicy::Drop
            && let Err(error) = admit(
                tx,
                session,
                run,
                std::slice::from_ref(&affected.input_id),
                &[],
            )?
        {
            return Ok(Err(error));
        }
        outcome.affected_inputs.push(affected);
    }
    Ok(Ok(outcome))
}

/// Withdraw the input session `session` would admit as run `run`, while it
/// is still open and unbound session mail: into its `cancelled` tombstone at
/// `now_ms`, so no run ever takes it (FIG-5262). `None` when no open input
/// names the run, or an admission bound it first.
pub(super) fn withdraw_queued(
    tx: &Connection,
    session: &SessionId,
    run: &TurnId,
    now_ms: i64,
) -> Answer<Option<InputId>> {
    let candidates = tx
        .statement(SQL.session_mail.open_inputs_named.sql())?
        .query_map([session.as_str(), run.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (input, source_key) in candidates {
        let input = match InputId::parse(input) {
            Ok(input) => input,
            Err(error) => return Ok(Err(undecodable("input id", error))),
        };
        if queued_input_run(&input, source_key.as_deref()).as_ref() != Some(run) {
            continue;
        }
        let withdrawn = tx
            .statement(SQL.session_mail.withdraw_input.sql())?
            .query_row(
                rusqlite::params![
                    session.as_str(),
                    input.as_str(),
                    lash_core_store::turn_input_vocabulary::TurnInputStateKind::Cancelled.as_str(),
                    now_ms,
                ],
                |_| Ok(()),
            )
            .optional()?;
        return Ok(Ok(withdrawn.map(|()| input)));
    }
    Ok(Ok(None))
}

/// Apply the owner's session-mail write: bind admitted mail to its run, or
/// mail the session its own next-turn input.
pub(super) fn apply(
    tx: &Connection,
    commit: &Committing<'_>,
    write: &SessionMailWrite,
) -> Answer<()> {
    match write {
        SessionMailWrite::Admit {
            session,
            run,
            inputs,
            batches,
        } => admit(tx, session, run, inputs, batches),
        SessionMailWrite::Enqueue {
            session,
            draft_json,
        } => enqueue(tx, commit, session, draft_json),
        SessionMailWrite::EnterOperation { session, batch } => {
            enter_operation(tx, commit, session, batch)
        }
    }
}

/// Enter plugin task `batch`'s operation run, its command still open.
fn enter_operation(
    tx: &Connection,
    commit: &Committing<'_>,
    session: &SessionId,
    batch: &BatchId,
) -> Answer<()> {
    let entered = tx
        .statement(SQL.session_mail.enter_operation.sql())?
        .query_row(
            rusqlite::params![session.as_str(), batch.as_str(), commit.now.0],
            |_| Ok(()),
        )
        .optional()?;
    Ok(match entered {
        Some(()) => Ok(()),
        None => Err(DurableError::Domain(
            DomainRefusal::SessionCommandWithdrawn {
                session: session.clone(),
                batch: batch.clone(),
            },
        )),
    })
}

/// Insert the owner's input as any producer's acceptance does, with the
/// session's wake, in the owner commit. A closing or gone session takes no
/// mail: its close settles what it holds, and the write inserts nothing.
fn enqueue(
    tx: &Connection,
    commit: &Committing<'_>,
    session: &SessionId,
    draft_json: &str,
) -> Answer<()> {
    let (live, closing) = standing(tx, session)?;
    if !live || closing {
        return Ok(Ok(()));
    }
    let refused = |reason: String| {
        Ok(Err(DurableError::Domain(
            DomainRefusal::SessionMailRefused {
                session: session.clone(),
                reason,
            },
        )))
    };
    let draft: lash_core_store::turn_input_vocabulary::PendingTurnInputDraft =
        match serde_json::from_str(draft_json) {
            Ok(draft) => draft,
            Err(error) => return Ok(Err(undecodable("owner input", error))),
        };
    let batch = match lash_core_store::turn_input_vocabulary::PendingTurnInputBatch::new(
        session.clone(),
        vec![draft],
    ) {
        Ok(batch) => batch,
        Err(error) => return refused(error.to_string()),
    };
    let now = u64::try_from(commit.now.0).unwrap_or(0);
    // The owner mails its own session: its next pass reads the input, so the
    // wake needs no hint.
    match crate::persistence::enqueue_pending_turn_inputs_conn(tx, &batch, now, 0) {
        Ok(_) => Ok(Ok(())),
        Err(lash_core_store::store::StoreError::Contended) => {
            Ok(Err(DurableError::Store(StoreFailure {
                code: None,
                kind: StoreFailureKind::Contended,
                message: "the owner's session mail contended".to_owned(),
            })))
        }
        Err(error) => refused(error.to_string()),
    }
}

/// Bind the admitted mail to its run, each row still open and unbound.
fn admit(
    tx: &Connection,
    session: &SessionId,
    run: &TurnId,
    inputs: &[InputId],
    batches: &[BatchId],
) -> Answer<()> {
    let moved = |item: &str| {
        Ok(Err(DurableError::Domain(DomainRefusal::SessionMailMoved {
            session: session.clone(),
            item: item.to_owned(),
        })))
    };
    for (statement, ids) in [
        (
            &SQLITE.bind_inputs,
            inputs.iter().map(InputId::as_str).collect::<Vec<_>>(),
        ),
        (
            &SQLITE.bind_batches,
            batches.iter().map(BatchId::as_str).collect::<Vec<_>>(),
        ),
    ] {
        if ids.is_empty() {
            continue;
        }
        let bound = tx
            .statement(statement.sql())?
            .query_map(
                rusqlite::params![
                    session.as_str(),
                    super::graph::json_array(&ids),
                    run.as_str()
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<std::collections::BTreeSet<_>>>()?;
        // Preserve the original first-moved identity and tolerate duplicate
        // names, as rebinding the same row repeatedly did.
        if let Some(id) = ids.iter().find(|id| !bound.contains(**id)) {
            return moved(id);
        }
    }
    Ok(Ok(()))
}

/// Arm (`true`) or disarm a cut of every session-mail producer transaction
/// at the session actor's wake: while armed, the wake's `mail_seq` bump
/// aborts, so the producer's transaction rolls back before it commits. The
/// mail atomicity law's fault.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn cut_session_wakes(conn: &Connection, armed: bool) -> rusqlite::Result<()> {
    conn.execute_batch(if armed {
        "CREATE TRIGGER lash_test_cut_wake BEFORE UPDATE OF mail_seq ON actors
         BEGIN SELECT RAISE(ABORT, 'cut before the producer commit'); END;"
    } else {
        "DROP TRIGGER lash_test_cut_wake"
    })
}

/// Project the production mailbox's bound-run probe for its history-work laws.
#[cfg(test)]
pub(crate) fn bound_run_query() -> String {
    format!(
        "SELECT bound_run FROM ({}) AS mailbox WHERE bound_run IS NOT NULL",
        SQLITE.mailbox.sql()
    )
}

#[cfg(test)]
pub(crate) fn open_inputs_query() -> String {
    format!(
        "SELECT item.value ->> 0 FROM ({}) AS mailbox JOIN json_each(mailbox.inputs) AS item ORDER BY CAST(item.key AS INTEGER)",
        SQLITE.mailbox.sql()
    )
}

#[cfg(test)]
mod tests {
    use crate::Prepare as _;
    use lash_core_store::store::{
        ControlIntentStore as _, QueuedWorkStore as _, SessionCatalogStore as _,
    };
    use lash_durable::domain::{DomainWrite, OperationRun, TurnWrite};
    use lash_durable::{
        ActorKey, CommitLabel, DurableReads as _, DurableStore as _, NodeId, NodeSpec,
    };

    /// CLEANUP-LIVENESS: only a live unbound command or its exact unfinished
    /// run can defer cleanup. Closing is live, deletion wins over a command,
    /// and a run retains its own authority until it ends.
    #[tokio::test]
    async fn cleanup_liveness_joins_the_exact_open_command_and_unfinished_run() {
        let set = crate::SqliteStoreSet::memory().await.unwrap();
        let store = set.durable_store();
        let factory = set.session_store_factory();
        let session = lash_kernel_value::identity::SessionId::fixture("cleanup-liveness");
        let absent = OperationRun {
            session_id: session.clone(),
            operation_id: "absent".to_owned(),
        };
        assert!(!store.operation_is_open(&absent).await.unwrap());
        factory
            .admit_session(&lash_core::testing::store_fixtures::root_session_request(
                &session,
            ))
            .await
            .unwrap();
        let batch = factory
            .enqueue_queued_work(lash_core::runtime::QueuedWorkBatchDraft::new(
                session.clone(),
                lash_core::DeliveryPolicy::EarliestSafeBoundary,
                lash_core::facade_support::SessionCommand::RefreshToolCatalog {
                    reason: "cleanup liveness".to_owned(),
                },
            ))
            .await
            .unwrap();
        let operation = OperationRun {
            session_id: session.clone(),
            operation_id: batch.batch_id.to_string(),
        };
        assert!(store.operation_is_open(&operation).await.unwrap());
        assert!(!store.operation_is_open(&absent).await.unwrap());
        factory.begin_session_close(&session, 1).await.unwrap();
        assert!(
            store.operation_is_open(&operation).await.unwrap(),
            "a closing session is live"
        );

        const DELETED: &str =
            "INSERT INTO deleted_sessions (session_id, created_at_ms, head_revision, relation_kind)
            VALUES ('cleanup-liveness', 0, 0, 'root')";
        store
            .conn
            .call(|connection| connection.execute_once(DELETED, []))
            .await
            .unwrap();
        assert!(
            !store.operation_is_open(&operation).await.unwrap(),
            "a deleted session's command is not open"
        );
        store
            .conn
            .call(|connection| {
                connection.execute_once(
                    "DELETE FROM deleted_sessions WHERE session_id = 'cleanup-liveness'",
                    [],
                )
            })
            .await
            .unwrap();
        let run = operation.run_id();
        let bound = run.clone();
        store.conn.call(move |connection| connection.execute_once(
            "UPDATE queued_work_batches SET admitted_run = ?1, admitted_by = ?1 WHERE session_id = 'cleanup-liveness'",
            [bound.as_str()],
        )).await.unwrap();
        assert!(
            !store.operation_is_open(&operation).await.unwrap(),
            "a bound batch alone cannot defer cleanup"
        );

        let actor = ActorKey::session(session.as_str()).unwrap();
        let formats = store.actor(&actor).await.unwrap().unwrap().formats;
        let node = store
            .register_node(&NodeSpec {
                node: NodeId::new("cleanup-liveness"),
                decodes: vec![formats],
                serves: Vec::new(),
                capabilities: Default::default(),
                ttl_millis: 60_000,
            })
            .await
            .unwrap();
        let epoch = store.claim(&node, 1).await.unwrap()[0].epoch;
        let mut admit = store.begin(&actor, epoch, u64::MAX).await.unwrap();
        admit.write(DomainWrite::Turn(TurnWrite::Admit {
            session: session.clone(),
            run: run.clone(),
            admission: lash_core_store::store::RunAdmissionRecord::Command {
                batch: batch.batch_id,
            },
            turn_deadline: None,
        }));
        store
            .commit(admit, CommitLabel::new("law.cleanup.admit"))
            .await
            .unwrap();
        assert!(store.operation_is_open(&operation).await.unwrap());
        assert!(
            !store.operation_is_open(&absent).await.unwrap(),
            "another operation's unfinished run does not match"
        );
        store
            .conn
            .call(|connection| connection.execute_once(DELETED, []))
            .await
            .unwrap();
        assert!(
            store.operation_is_open(&operation).await.unwrap(),
            "the unfinished run survives deletion independently"
        );
        store.conn.call(|connection| connection.execute_once(
            "UPDATE queued_work_batches SET admitted_run = NULL, admitted_by = NULL,
                terminal_cause = 'applied', terminal_at_ms = 1 WHERE session_id = 'cleanup-liveness'", [],
        )).await.unwrap();
        assert!(
            store.operation_is_open(&operation).await.unwrap(),
            "a settled command does not end its unfinished run"
        );
        let mut end = store.begin(&actor, epoch, u64::MAX).await.unwrap();
        end.write(DomainWrite::Turn(TurnWrite::Terminal {
            session,
            run,
            cause: Box::new(lash_core_store::store::RunTerminalCause::CommandsApplied),
            head_revision: None,
            transcript: None,
        }));
        store
            .commit(end, CommitLabel::new("law.cleanup.end"))
            .await
            .unwrap();
        assert!(!store.operation_is_open(&operation).await.unwrap());
    }
}
