//! Park-id CAS and the store half of root control, in one write transaction.
use lash_core_execution::store::*;

use crate::session_roots::*;
use crate::support::store_sqlx_error;

pub(crate) async fn open_root_intent_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &RootIntentRequest,
    at_ms: u64,
) -> Result<ControlIntent, RootIntentRefused> {
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
            RootIntentRefused::SessionDeleted
        } else {
            RootIntentRefused::NotParked
        });
    };
    let park = crate::runtime_persistence::turn_park::turn_park_for_update(tx, session).await?;
    let resume = match park.as_ref().and_then(|p| p.resume_intent) {
        Some(id) => load_intent_conn(tx, id).await?,
        None => None,
    };
    let verbs = open_verbs_by_session_conn(tx, session).await?;
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
    let mut intent =
        insert_intent_conn(tx, session, kind, plan.park.engine.as_ref(), at_ms).await?;
    let park_sql = &crate::turn_ingress::turn_ingress_sql().turn_parks;
    if request.verb == RootVerb::Redrive {
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
            request.root.as_str(),
            request.park.feed_sequence() as i64,
            &ParkEventKind::RedriveRequested { intent: intent.id },
            at_ms,
        )
        .await?;
        return Ok(intent);
    }
    let sql = &crate::session_roots::session_roots_sql().verbs;
    let mut run = crate::runtime_persistence::queued_run::load_run_tx(tx, session, None)
        .await?
        .filter(|run| run.scope.id() == request.root.as_str());
    let new_root = (request.verb == RootVerb::Fork && run.is_none())
        .then(|| forked_root(&request.root, intent.id));
    if request.verb == RootVerb::Fork {
        intent.kind = ControlIntentKind::Fork {
            root: request.root.clone(),
            park: request.park,
            new_root: new_root.clone(),
        };
        sqlx::query(sql.set_kind.sql())
            .bind(intent.id.sequence() as i64)
            .bind(stored_intent_kind(&intent.kind)?)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    let cause = match request.verb {
        RootVerb::Fork => RootTerminalCause::Forked {
            intent: intent.id,
            new_root: new_root.clone(),
        },
        _ => RootTerminalCause::OperatorCancelled { intent: intent.id },
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
    )
    .await?;
    let mut inputs: Vec<String> = sqlx::query_scalar(sql.bound_inputs.sql())
        .bind(session.as_str())
        .bind(request.root.as_str())
        .fetch_all(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
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
        sqlx::query(sql.input.sql())
            .bind(session.as_str())
            .bind(&input)
            .bind(state)
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
        if let Some(root) = new_root.as_ref() {
            sqlx::query(sql.rebind.sql())
                .bind(session.as_str())
                .bind(&input)
                .bind(root.as_str())
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        } else if request.verb == RootVerb::Fork {
            sqlx::query(sql.unbind.sql())
                .bind(session.as_str())
                .bind(&input)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        }
    }
    if request.verb == RootVerb::Cancel {
        for batch in batches {
            sqlx::query(sql.delete_batch_items.sql())
                .bind(&batch)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            sqlx::query(sql.delete_batch.sql())
                .bind(session.as_str())
                .bind(&batch)
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
        }
    }
    for statement in [sql.release_inputs.sql(), sql.release_batches.sql()] {
        sqlx::query(statement)
            .bind(session.as_str())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
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
        crate::runtime_persistence::queued_run::write_run_tx(tx, run, false).await?;
    }
    for prior in plan.supersede {
        let mut next = prior.clone();
        next.state = ControlIntentState::Superseded { by: intent.id };
        if !write_intent_state_conn(tx, &prior, &next).await? {
            return Err(StoreError::Contended.into());
        }
    }
    sqlx::query(park_sql.delete_for_turn_returning.sql())
        .bind(session.as_str())
        .bind(request.root.as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    let cause = if request.verb == RootVerb::Fork {
        ParkCancelCause::Forked {
            intent: intent.id,
            new_root,
        }
    } else {
        ParkCancelCause::Operator { intent: intent.id }
    };
    crate::runtime_persistence::turn_park_feed::log_turn_park_closed_tx(
        tx,
        session,
        request.root.as_str(),
        request.park.feed_sequence() as i64,
        &ParkEventKind::Cancelled { cause },
        at_ms,
    )
    .await?;
    sqlx::query(sql.raise_epoch.sql())
        .bind(session.as_str())
        .bind(close_admission(intent.id).as_str())
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(intent)
}
