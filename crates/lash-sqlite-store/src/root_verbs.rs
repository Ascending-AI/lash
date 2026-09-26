//! Park-id CAS and the store half of root control, in one write transaction.
use lash_core_execution::store::*;
use rusqlite::{Connection, OptionalExtension, params};

use crate::session_roots::*;
use crate::sqlite_error;

pub(crate) fn open_root_intent_conn(
    tx: &Connection,
    request: &RootIntentRequest,
    at_ms: u64,
) -> Result<ControlIntent, RootIntentRefused> {
    let session = &request.session_id;
    let closing: Option<Option<i64>> = tx
        .query_row(
            crate::session_sql::session_sql()
                .meta
                .select_closing_intent
                .sql(),
            params![session.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    let Some(closing) = closing else {
        return Err(if close_session_intent_conn(tx, session)?.is_some() {
            RootIntentRefused::SessionDeleted
        } else {
            RootIntentRefused::NotParked
        });
    };
    let park = crate::persistence::turn_park::turn_park_conn(tx, session)?;
    let resume = match park.as_ref().and_then(|p| p.resume_intent) {
        Some(id) => load_intent_conn(tx, id)?,
        None => None,
    };
    let verbs = open_verbs_by_session_conn(tx, session)?;
    let plan = decide_root_intent(
        request,
        &RootIntentFacts {
            closing: closing.map(|id| ControlIntentId::from_sequence(id as u64)),
            park: park.as_ref(),
            open_verbs: &verbs,
            resume: resume.as_ref(),
        },
    )?;
    let kind = match request.verb {
        RootVerb::Redrive => ControlIntentKind::Redrive {
            root: request.root.clone(),
            park: request.park,
        },
        RootVerb::Cancel => ControlIntentKind::Cancel {
            root: request.root.clone(),
            park: request.park,
        },
        RootVerb::Fork => ControlIntentKind::Fork {
            root: request.root.clone(),
            park: request.park,
            new_root: None,
        },
    };
    let mut intent = insert_intent_conn(tx, session, kind, plan.park.engine.as_ref(), at_ms)?;
    let park_sql = &crate::turn_ingress::turn_ingress_sql().turn_parks;
    if request.verb == RootVerb::Redrive {
        tx.execute(
            park_sql.set_resume_intent.sql(),
            params![
                session.as_str(),
                request.park.feed_sequence() as i64,
                intent.id.sequence() as i64
            ],
        )
        .map_err(sqlite_error)?;
        crate::persistence::turn_park_feed::log_turn_park_closed_conn(
            tx,
            session,
            request.root.as_str(),
            request.park.feed_sequence() as i64,
            &ParkEventKind::RedriveRequested { intent: intent.id },
            crate::clamp_epoch_ms(at_ms),
        )?;
        return Ok(intent);
    }
    let sql = &crate::session_roots::session_roots_sql().verbs;
    let mut run = crate::persistence::queued_run::load_run_conn(tx, session, None)?
        .filter(|run| run.scope.id() == request.root.as_str());
    let new_root = (request.verb == RootVerb::Fork && run.is_none())
        .then(|| forked_root(&request.root, intent.id));
    if request.verb == RootVerb::Fork {
        intent.kind = ControlIntentKind::Fork {
            root: request.root.clone(),
            park: request.park,
            new_root: new_root.clone(),
        };
        tx.execute(
            sql.set_kind.sql(),
            params![
                intent.id.sequence() as i64,
                stored_intent_kind(&intent.kind)?
            ],
        )
        .map_err(sqlite_error)?;
    }
    let cause = match request.verb {
        RootVerb::Fork => RootTerminalCause::Forked {
            intent: intent.id,
            new_root: new_root.clone(),
        },
        _ => RootTerminalCause::OperatorCancelled { intent: intent.id },
    };
    let head = &crate::session_sql::session_sql().head;
    let revision: Option<i64> = tx
        .query_row(
            head.select_revision.sql(),
            params![session.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_error)?;
    tx.execute(
        head.clear_pending_follow_on.sql(),
        params![session.as_str()],
    )
    .map_err(sqlite_error)?;
    write_root_terminal_conn(
        tx,
        &RootTerminal {
            session_id: session.clone(),
            root: request.root.clone(),
            kind: RootTerminalKind::Cancelled,
            cause,
            head_revision: revision.map(|revision| revision as u64),
            at_ms,
        },
    )?;
    let mut inputs: Vec<String> = {
        let mut stmt = tx.prepare(sql.bound_inputs.sql()).map_err(sqlite_error)?;
        stmt.query_map(params![session.as_str(), request.root.as_str()], |row| {
            row.get(0)
        })
        .map_err(sqlite_error)?
        .collect::<Result<_, _>>()
        .map_err(sqlite_error)?
    };
    let mut batches = Vec::new();
    if let Some(run) = run.as_ref() {
        for member in run
            .members
            .iter()
            .flatten()
            .chain(run.withheld_members.iter())
        {
            match member {
                QueuedRunMember::Input(id) => inputs.push(id.to_string()),
                QueuedRunMember::Batch(id) => batches.push(id.to_string()),
            }
        }
    }
    inputs.sort();
    inputs.dedup();
    for input in inputs {
        let state = if request.verb == RootVerb::Fork {
            "deferred_next_turn"
        } else {
            "cancelled"
        };
        tx.execute(sql.input.sql(), params![session.as_str(), input, state])
            .map_err(sqlite_error)?;
        if let Some(root) = new_root.as_ref() {
            tx.execute(
                sql.rebind.sql(),
                params![session.as_str(), input, root.as_str()],
            )
            .map_err(sqlite_error)?;
        } else if request.verb == RootVerb::Fork {
            tx.execute(sql.unbind.sql(), params![session.as_str(), input])
                .map_err(sqlite_error)?;
        }
    }
    if request.verb == RootVerb::Cancel {
        for batch in batches {
            tx.execute(sql.delete_batch_items.sql(), params![batch])
                .map_err(sqlite_error)?;
            tx.execute(sql.delete_batch.sql(), params![session.as_str(), batch])
                .map_err(sqlite_error)?;
        }
    }
    for statement in [sql.release_inputs.sql(), sql.release_batches.sql()] {
        tx.execute(statement, params![session.as_str()])
            .map_err(sqlite_error)?;
    }
    if let Some(run) = run.as_mut() {
        run.revision += 1;
        run.terminal = Some(QueuedRunTerminal::Completed {
            turn_id: run.position.turn_id.clone(),
            outcome: lash_sansio::TurnOutcome::Stopped(lash_sansio::TurnStop::Cancelled {
                evidence: lash_sansio::TurnCancellationEvidence::internal(format!(
                    "intent:{}",
                    intent.id
                )),
            }),
        });
        crate::persistence::queued_run::write_run_conn(tx, run, false)?;
    }
    for prior in plan.supersede {
        let mut next = prior.clone();
        next.state = ControlIntentState::Superseded { by: intent.id };
        if !write_intent_state_conn(tx, &prior, &next)? {
            return Err(StoreError::Contended.into());
        }
    }
    tx.query_row(
        park_sql.delete_for_turn_returning.sql(),
        params![session.as_str(), request.root.as_str()],
        |_| Ok(()),
    )
    .map_err(sqlite_error)?;
    let cause = if request.verb == RootVerb::Fork {
        ParkCancelCause::Forked {
            intent: intent.id,
            new_root,
        }
    } else {
        ParkCancelCause::Operator { intent: intent.id }
    };
    crate::persistence::turn_park_feed::log_turn_park_closed_conn(
        tx,
        session,
        request.root.as_str(),
        request.park.feed_sequence() as i64,
        &ParkEventKind::Cancelled { cause },
        crate::clamp_epoch_ms(at_ms),
    )?;
    tx.execute(
        sql.raise_epoch.sql(),
        params![session.as_str(), close_admission(intent.id).as_str()],
    )
    .map_err(sqlite_error)?;
    Ok(intent)
}
