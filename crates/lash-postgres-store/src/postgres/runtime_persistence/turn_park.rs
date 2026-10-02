//! A session's park, root-addressed (FIG-3600 S7, D2 §1.3): the one record
//! a parked root holds, read and written inside the caller's transaction.
//!
//! Every write is decided by [`decide_turn_park_write`] against the stored
//! park, read under a row lock, after the root's terminal evidence was found
//! absent: a root with terminal evidence never parks (P2), so a zombie
//! execution of a root an operator already ended leaves nothing behind.

use lash_core_execution::store::{
    ControlIntent, ControlIntentId, ControlIntentState, ParkEventKind, ParkId, StoreError,
    StoredParkRedrive, StoredTurnParkHead, TurnPark, TurnParkWrite, TurnParkWriteDecision,
    UnparkCause, decide_turn_park_write,
};
use lash_sansio::SessionId;
use sqlx::{PgConnection, Row};

use crate::runtime_persistence::turn_park_feed::{log_turn_park_closed_tx, log_turn_parked_tx};
use crate::support::store_sqlx_error;

fn sql_i64(field: &str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Backend(format!("{field} {value} exceeds the stored range")))
}

fn stored_u64(field: &str, value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::StoredDataCorrupt {
        record_kind: "TurnPark",
        message: format!("negative {field}"),
    })
}

/// Decode a `turn_parks` row read with the record projection.
pub(crate) fn decode_turn_park_row(row: &sqlx::postgres::PgRow) -> Result<TurnPark, StoreError> {
    let session_id: String = row.try_get(0).map_err(store_sqlx_error)?;
    let root: String = row.try_get(1).map_err(store_sqlx_error)?;
    let park_id: i64 = row.try_get(2).map_err(store_sqlx_error)?;
    let reason_code: String = row.try_get(3).map_err(store_sqlx_error)?;
    let reason_json: String = row.try_get(4).map_err(store_sqlx_error)?;
    let since_ms: i64 = row.try_get(5).map_err(store_sqlx_error)?;
    let last_refused_ms: i64 = row.try_get(6).map_err(store_sqlx_error)?;
    let attempts: i64 = row.try_get(7).map_err(store_sqlx_error)?;
    let engine_ref: Option<String> = row.try_get(8).map_err(store_sqlx_error)?;
    let resume_intent: Option<i64> = row.try_get(9).map_err(store_sqlx_error)?;
    let build_generation: Option<String> = row.try_get(10).map_err(store_sqlx_error)?;
    let child_engine_refs: Option<String> = row.try_get(11).map_err(store_sqlx_error)?;
    TurnPark::decode(
        SessionId::from(session_id),
        root.into(),
        ParkId::from_feed_sequence(stored_u64("park_id", park_id)?),
        &reason_code,
        &reason_json,
        stored_u64("since_ms", since_ms)?,
        stored_u64("last_refused_ms", last_refused_ms)?,
        u32::try_from(attempts).unwrap_or(u32::MAX),
        engine_ref,
        child_engine_refs.as_deref(),
        resume_intent
            .map(|intent| stored_u64("resume_intent", intent))
            .transpose()?,
        build_generation.as_deref(),
    )
}

/// `session_id`'s park under a row lock, read on `conn`: the write that
/// follows decides on what this read saw.
pub(crate) async fn turn_park_for_update(
    conn: &mut PgConnection,
    session_id: &SessionId,
) -> Result<Option<TurnPark>, StoreError> {
    sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .turn_parks_postgres
            .select_for_update_by_session
            .sql(),
    )
    .bind(session_id.as_str())
    .fetch_optional(&mut *conn)
    .await
    .map_err(store_sqlx_error)?
    .as_ref()
    .map(decode_turn_park_row)
    .transpose()
}

/// Add the child handle `write` reports to the ones `park` records, inside
/// `tx`.
async fn record_child_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    park: &mut TurnPark,
    write: &TurnParkWrite,
) -> Result<(), StoreError> {
    let Some(child) = write.child().filter(|child| !park.children.contains(child)) else {
        return Ok(());
    };
    park.children.push(child.clone());
    sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .turn_parks
            .set_children
            .sql(),
    )
    .bind(park.session_id.as_str())
    .bind(park.turn_id.as_str())
    .bind(TurnPark::encode_children(&park.children)?)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

/// Record `write` inside `tx`: refuse a root with terminal evidence (P2),
/// then open, supersede, re-park or attach the engine's handle as
/// [`decide_turn_park_write`] rules.
pub(crate) async fn record_turn_park_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    write: &TurnParkWrite,
) -> Result<TurnPark, StoreError> {
    let session_id = &write.session_id;
    super::lock_session_history_mutation_tx(tx, session_id).await?;
    let turn_parks = &crate::turn_ingress::turn_ingress_sql().turn_parks;
    if let Some(terminal) =
        crate::session_roots::root_terminal_conn(tx, session_id, &write.turn_id).await?
    {
        return Err(StoreError::RootAlreadyTerminal {
            session_id: session_id.clone(),
            root: write.turn_id.clone(),
            by: Box::new(terminal.cause),
        });
    }
    // Withdrawal locks input rows before deleting the park. Take these locks
    // first too, so either withdrawal clears this park or the stale writer
    // observes that every bound input was cancelled.
    let states: Vec<String> = sqlx::query_scalar(
        crate::turn_ingress::turn_ingress_sql()
            .family_postgres
            .root_bound_input_states
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(write.turn_id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    if !states.is_empty()
        && states.iter().all(|state| {
            state == lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str()
        })
    {
        return Err(StoreError::RootInputWithdrawn {
            session_id: session_id.clone(),
            root: write.turn_id.clone(),
        });
    }
    let stored = turn_park_for_update(tx, session_id).await?;
    let reason_code = write.reason.code().as_str();
    let reason_json =
        serde_json::to_string(&write.reason).map_err(|error| StoreError::RecordEncodingFailed {
            record_kind: "TurnPark".to_string(),
            message: error.to_string(),
        })?;
    let at_ms = sql_i64("turn park instant", write.at_ms)?;
    let engine_ref = write.engine().map(|engine| engine.as_str().to_string());
    let park_executable_generation = write
        .reason
        .retired_executable_generation_key()
        .map(str::to_string);
    let park_build_generation = write
        .build_generation
        .as_ref()
        .map(|generation| generation.as_str().to_string());
    let (head, redrive) = match stored.as_ref() {
        Some(park) => {
            let redrive = match park.resume_intent {
                Some(intent) => crate::session_roots::load_intent_conn(tx, intent).await?,
                None => None,
            };
            let head = StoredTurnParkHead {
                root: park.turn_id.clone(),
                engine: park.engine.clone(),
                children: park.children.clone(),
                redrive: park.resume_intent.map(|intent| StoredParkRedrive {
                    intent,
                    open: redrive
                        .as_ref()
                        .is_some_and(ControlIntent::engine_half_owed),
                }),
            };
            (Some(head), redrive)
        }
        None => (None, None),
    };
    match (decide_turn_park_write(head.as_ref(), write), stored) {
        (TurnParkWriteDecision::Unchanged, Some(park)) => return Ok(park),
        (TurnParkWriteDecision::AttachEngine, Some(mut park)) => {
            sqlx::query(turn_parks.attach_engine.sql())
                .bind(session_id.as_str())
                .bind(write.turn_id.as_str())
                .bind(engine_ref.as_deref())
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            park.engine = write.engine().cloned();
            return Ok(park);
        }
        (TurnParkWriteDecision::AttachChild, Some(mut park)) => {
            record_child_tx(tx, &mut park, write).await?;
            return Ok(park);
        }
        (TurnParkWriteDecision::Repark, Some(mut park)) => {
            // A same-root re-park keeps `park_id` and `since_ms`, refreshes
            // the reason and `last_refused_ms`, counts the refusal and
            // clears a requested redrive — no feed event.
            sqlx::query(turn_parks.update_same_turn.sql())
                .bind(session_id.as_str())
                .bind(write.turn_id.as_str())
                .bind(reason_code)
                .bind(&reason_json)
                .bind(at_ms)
                .bind(park_executable_generation.as_deref())
                .bind(engine_ref.as_deref())
                .bind(park_build_generation.as_deref())
                .execute(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            // The root ran past a redrive the park still named open: its
            // resume reached the execution and only its acknowledgement was
            // lost. Settle it here, so it never resumes the root again. Only
            // the root's own refusal re-parks past an open redrive: a
            // reconcile write is decided `Unchanged` against one.
            if let Some(open) =
                redrive.filter(|redrive| matches!(redrive.state, ControlIntentState::Pending))
            {
                let mut settled = open.clone();
                settled.state = ControlIntentState::Acknowledged { at_ms: write.at_ms };
                if !crate::session_roots::write_intent_state_conn(tx, &open, &settled).await? {
                    return Err(StoreError::Contended);
                }
            }
            park.reason = write.reason.clone();
            park.last_refused_ms = write.at_ms;
            park.attempts = park.attempts.saturating_add(1);
            park.resume_intent = None;
            if let Some(engine) = write.engine() {
                park.engine = Some(engine.clone());
            }
            // COALESCE on the row: a stamp-less write keeps the recorded
            // generation.
            if write.build_generation.is_some() {
                park.build_generation = write.build_generation.clone();
            }
            record_child_tx(tx, &mut park, write).await?;
            return Ok(park);
        }
        (TurnParkWriteDecision::Supersede, Some(superseded)) => {
            // A different root's park supersedes the stored one: close it on
            // the feed, then open the new park.
            let deleted = sqlx::query(turn_parks.delete_for_supersede_returning.sql())
                .bind(session_id.as_str())
                .bind(write.turn_id.as_str())
                .fetch_optional(&mut **tx)
                .await
                .map_err(store_sqlx_error)?;
            if deleted.is_none() {
                return Err(StoreError::Backend(format!(
                    "turn park supersede of `{}` in session `{session_id}` deleted no row",
                    superseded.turn_id
                )));
            }
            log_turn_park_closed_tx(
                tx,
                session_id,
                superseded.turn_id.as_str(),
                sql_i64("park id", superseded.park_id.feed_sequence())?,
                &ParkEventKind::Unparked {
                    cause: UnparkCause::Superseded,
                },
                write.at_ms,
            )
            .await?;
        }
        (TurnParkWriteDecision::Open, None) => {}
        (decision, stored) => {
            return Err(StoreError::Backend(format!(
                "turn park write decided {decision:?} against stored park {:?}",
                stored.map(|park| park.park_id)
            )));
        }
    }
    let children: Vec<_> = write.child().cloned().into_iter().collect();
    let park_id = log_turn_parked_tx(
        tx,
        session_id,
        write.turn_id.as_str(),
        &write.reason,
        write.at_ms,
        park_build_generation.as_deref(),
    )
    .await?;
    sqlx::query(turn_parks.insert.sql())
        .bind(session_id.as_str())
        .bind(write.turn_id.as_str())
        .bind(park_id)
        .bind(reason_code)
        .bind(&reason_json)
        .bind(at_ms)
        .bind(at_ms)
        .bind(1_i64)
        .bind(park_executable_generation.as_deref())
        .bind(engine_ref.as_deref())
        .bind(park_build_generation.as_deref())
        .bind(TurnPark::encode_children(&children)?)
        .execute(&mut **tx)
        .await
        .map_err(store_sqlx_error)?;
    Ok(TurnPark {
        session_id: session_id.clone(),
        turn_id: write.turn_id.clone(),
        reason: write.reason.clone(),
        park_id: ParkId::from_feed_sequence(u64::try_from(park_id).unwrap_or_default()),
        since_ms: write.at_ms,
        last_refused_ms: write.at_ms,
        attempts: 1,
        engine: write.engine().cloned(),
        children,
        resume_intent: None,
        build_generation: write.build_generation.clone(),
    })
}

/// Record redrive `intent` on session `session_id`'s park `park_id`, inside
/// the redrive's store half.
#[expect(
    dead_code,
    reason = "the redrive's store half lands with the parked-root verbs"
)]
pub(crate) async fn set_resume_intent_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    park_id: ParkId,
    intent: ControlIntentId,
) -> Result<(), StoreError> {
    sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .turn_parks
            .set_resume_intent
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(sql_i64("park id", park_id.feed_sequence())?)
    .bind(sql_i64("control intent id", intent.sequence())?)
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

/// End `root`'s park on `conn` (inside the transaction that writes the
/// root's terminal), with the feed event its end `cause` names (FIG-4780).
/// A root's end is what ends its park, whatever kind of root it is: a
/// command root commits no turn, so no commit would. A session parked on
/// another root, or on none, is left as it is and appends nothing.
pub(crate) async fn end_root_park_conn(
    conn: &mut PgConnection,
    session_id: &SessionId,
    root: &lash_sansio::TurnId,
    cause: &lash_core_execution::store::RootTerminalCause,
    at_ms: u64,
) -> Result<(), StoreError> {
    let ended = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .turn_parks
            .delete_for_turn_returning
            .sql(),
    )
    .bind(session_id.as_str())
    .bind(root.as_str())
    .fetch_optional(&mut *conn)
    .await
    .map_err(store_sqlx_error)?;
    let Some(ended) = ended else {
        return Ok(());
    };
    let park_id: i64 = ended.try_get(1).map_err(store_sqlx_error)?;
    log_turn_park_closed_tx(
        conn,
        session_id,
        root.as_str(),
        park_id,
        &ParkEventKind::ending_root_park(cause),
        at_ms,
    )
    .await
}
