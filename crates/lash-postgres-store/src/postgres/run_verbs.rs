//! Park-id CAS and the store half of run control, in one write transaction.
use lash_core_execution::store::*;

use crate::session_runs::*;
use crate::support::store_sqlx_error;

pub(crate) async fn open_run_intent_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &RunIntentRequest,
    at_ms: u64,
) -> Result<ControlIntent, RunIntentRefused> {
    let session = &request.session_id;
    crate::runtime_persistence::lock_session_history_mutation_tx(tx, session).await?;
    let closing: Option<Option<i64>> = sqlx::query_scalar(
        crate::session_sql::session_sql()
            .meta
            .select_closing_intent
            .sql(),
    )
    .bind(session.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    let Some(closing) = closing else {
        return Err(if close_session_intent_conn(tx, session).await?.is_some() {
            RunIntentRefused::SessionDeleted
        } else {
            RunIntentRefused::NotParked
        });
    };
    let park = crate::runtime_persistence::turn_park::turn_park_for_update(tx, session).await?;
    let resume = match park.as_ref().and_then(|p| p.resume_intent) {
        Some(id) => load_intent_conn(tx, id).await?,
        None => None,
    };
    let verbs = open_verbs_by_session_conn(tx, session).await?;
    let plan = decide_run_intent(
        request,
        &RunIntentFacts {
            closing: closing.map(|id| ControlIntentId::from_sequence(id as u64)),
            park: park.as_ref(),
            open_verbs: &verbs,
            resume: resume.as_ref(),
        },
    )?;
    let kind = match request.verb {
        RunVerb::Redrive => ControlIntentKind::Redrive {
            run: request.run.clone(),
            park: request.park,
            children: plan.park.children.clone(),
        },
        RunVerb::Cancel => ControlIntentKind::Cancel {
            run: request.run.clone(),
            park: request.park,
        },
        RunVerb::Fork => ControlIntentKind::Fork {
            run: request.run.clone(),
            park: request.park,
            new_run: None,
        },
    };
    let mut intent =
        insert_intent_conn(tx, session, kind, plan.park.engine.as_ref(), at_ms).await?;
    let park_sql = &crate::turn_ingress::turn_ingress_sql().turn_parks;
    if request.verb == RunVerb::Redrive {
        sqlx::query(park_sql.set_resume_intent.sql())
            .bind(session.as_str())
            .bind(request.park.feed_sequence() as i64)
            .bind(intent.id.sequence() as i64)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        crate::runtime_persistence::turn_park_feed::log_turn_park_closed_tx(
            tx,
            session,
            request.run.as_str(),
            request.park.feed_sequence() as i64,
            &ParkEventKind::RedriveRequested { intent: intent.id },
            at_ms,
        )
        .await?;
        return Ok(intent);
    }
    let sql = &crate::session_runs::session_runs_sql().verbs;
    // A queued-headed run's batches are its own: a cancel removes them, and
    // a fork leaves them queued with no new run to execute them.
    let batches = crate::session_runs::admitted_batches_conn(tx, session, &request.run).await?;
    let new_run = (request.verb == RunVerb::Fork && batches.is_empty())
        .then(|| forked_run(&request.run, intent.id));
    if request.verb == RunVerb::Fork {
        intent.kind = ControlIntentKind::Fork {
            run: request.run.clone(),
            park: request.park,
            new_run: new_run.clone(),
        };
        sqlx::query(sql.set_kind.sql())
            .bind(intent.id.sequence() as i64)
            .bind(stored_intent_kind(&intent.kind)?)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    let cause = match request.verb {
        RunVerb::Fork => RunTerminalCause::Forked {
            intent: intent.id,
            new_run: new_run.clone(),
        },
        _ => RunTerminalCause::OperatorCancelled { intent: intent.id },
    };
    let head = &crate::session_sql::session_sql().head;
    let revision: Option<i64> = sqlx::query_scalar(head.select_revision.sql())
        .bind(session.as_str())
        .fetch_optional(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    sqlx::query(head.clear_pending_follow_on.sql())
        .bind(session.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let mut inputs: Vec<String> = sqlx::query_scalar(sql.bound_inputs.sql())
        .bind(session.as_str())
        .bind(request.run.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    inputs.sort();
    inputs.dedup();
    for input in inputs {
        if request.verb == RunVerb::Fork {
            sqlx::query(sql.reopen_input.sql())
                .bind(session.as_str())
                .bind(&input)
        } else {
            sqlx::query(sql.cancel_input.sql())
                .bind(session.as_str())
                .bind(&input)
                .bind(crate::support::clamp_epoch_ms(at_ms))
                .bind(lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str())
        }
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
        if let Some(run) = new_run.as_ref() {
            sqlx::query(sql.rebind.sql())
                .bind(session.as_str())
                .bind(&input)
                .bind(run.as_str())
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        } else if request.verb == RunVerb::Fork {
            sqlx::query(sql.unbind.sql())
                .bind(session.as_str())
                .bind(&input)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        }
    }
    if request.verb == RunVerb::Cancel {
        for batch in batches {
            crate::session_runs::cancel_run_batch_tx(tx, session, &batch, at_ms).await?;
        }
    }
    // The verb settles the run's own input and batches first; its terminal
    // write then releases whatever else the run still held (FIG-3927) and
    // ends its park, as every run's end does (FIG-4780).
    write_run_terminal_conn(
        tx,
        &RunTerminal {
            session_id: session.clone(),
            run: request.run.clone(),
            cause,
            head_revision: revision.map(|revision| revision as u64),
            at_ms,
        },
    )
    .await?;
    for prior in plan.supersede {
        let mut next = prior.clone();
        next.state = ControlIntentState::Superseded { by: intent.id };
        if !write_intent_state_conn(tx, &prior, &next).await? {
            return Err(StoreError::Contended.into());
        }
    }
    sqlx::query(sql.raise_epoch.sql())
        .bind(session.as_str())
        .bind(close_admission(intent.id).as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(intent)
}
