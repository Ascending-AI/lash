//! One prepared registration applied on a caller's PostgreSQL transaction:
//! the registrar's own transaction (ADR 0132 §12).

use super::*;

use crate::guarded_tx::GuardedTx;

/// What applying one prepared registration did.
pub(crate) enum AppliedRegistration {
    /// The row this registration inserted, with its observers and its
    /// actor, ready.
    Created(ProcessRecord),
    /// The process a retained start under the key already holds, and its
    /// wake session; nothing was written.
    Retained { record: ProcessRecord },
    /// A concurrent start under the same key won the insert: the caller rolls
    /// its transaction back, and the winner is the start's process.
    LostRace { winner: ProcessRecord },
}

/// Apply one prepared registration on `tx`.
///
/// # Errors
///
/// A stale preparation, a refused start (a closed scope, an abandoned
/// consumer hold) and any store failure.
pub(crate) async fn apply_registration_tx(
    tx: &mut GuardedTx<'_>,
    registration: ProcessRegistration,
    observers: Vec<SessionId>,
    process_id: ProcessId,
    retained: bool,
    now: u64,
    fleet: lash_core_execution::FleetFormat,
) -> Result<AppliedRegistration, PluginError> {
    let mut observers = observers;
    observers.sort();
    observers.dedup();
    let consumer_hold = registration.consumer_hold.clone();
    let start_key = registration.start_key.clone();
    // While the process minted for a key is retained, a start under the
    // same key returns that process untouched (ADR 0107); a host's key
    // must also present its start, with equal content.
    if let Some(start_key) = start_key.as_ref()
        && let Some(existing) = load_process_by_start_key_tx(tx, start_key).await?
    {
        if retained && existing.id != process_id {
            return Err(
                lash_core_execution::StoreError::PreparedProcessRegistrationStale {
                    process_id: process_id.clone(),
                }
                .into(),
            );
        }
        return Ok(AppliedRegistration::Retained { record: existing });
    }
    if retained {
        return Err(
            lash_core_execution::StoreError::PreparedProcessRegistrationStale {
                process_id: process_id.clone(),
            }
            .into(),
        );
    }
    let registration = lash_core_execution::runtime::prepare_process_registration(registration)?;
    // Admission against closure (FIG-3607 R11): a new start is refused
    // once its starter has ended, whatever its own lifetime, once the
    // scope its lifetime names has closed, and once the session either
    // lies inside has closed (FIG-3948).
    //
    // The reads and this transaction's insert are one decision, so each
    // is taken under its scope's advisory lock. Without it the pair is a
    // check-then-act against a close written in its own transaction on
    // another connection: the start would read "no row", the row would
    // commit, the sweep would page children without seeing this
    // uncommitted one, and the child would land live under a closed
    // scope. Holding the lock orders the two writes either way round. The
    // locks are taken in key order, so two starts never wait on each
    // other's second lock.
    let fenced = registration.closing_scopes();
    for scope in &fenced {
        parent_end::lock_parent_scope_tx(tx, scope).await?;
    }
    for scope in fenced {
        if parent_end::plan_exists_tx(tx, &scope).await? {
            return Err(PluginError::ParentEnded {
                start_key: registration.start_key.clone(),
                parent: scope,
            });
        }
    }
    // A start whose consuming call was abandoned is refused: the call's
    // opener already drained what the hold owed (ADR 0116 §3.4). The
    // hold's lock orders this read against the abandonment's mark.
    if let Some(hold) = consumer_hold.as_ref() {
        parent_end::lock_consumer_hold_tx(tx, &hold.key).await?;
        let abandoned: bool = sqlx::query_scalar(process_sql().abandoned_hold.exists.sql())
            .bind(hold.key.as_str())
            .fetch_one(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(plugin_sqlx_error)?;
        if abandoned {
            return Err(lash_core_execution::runtime::abandoned_consumer_refusal(
                registration.start_key.as_ref(),
                &hold.key,
            ));
        }
    }
    let mut record = ProcessRecord::from_prepared_registration(registration, process_id, now);
    let record_json = serde_json::to_string(&record).map_err(process_decode_error)?;
    let result = sqlx::query(process_sql().process_postgres.insert_registration.sql())
        .bind(record.id.as_str())
        .bind(
            record
                .start_key
                .as_ref()
                .map(lash_core_execution::StartKey::as_str),
        )
        .bind(record.originator_id().as_str())
        .bind(record.identity.kind.as_str())
        .bind(&record.identity.label)
        .bind(record.created_at_ms as i64)
        .bind(record.updated_at_ms as i64)
        .bind(record.last_event_sequence as i64)
        .bind(process_status_label(&record))
        .bind(
            record
                .lifetime
                .scope()
                .map(lash_core_execution::ScopeId::storage_kind),
        )
        .bind(
            record
                .lifetime
                .scope()
                .map(lash_core_execution::ScopeId::storage_id),
        )
        .bind(record.lifetime.storage_label())
        .bind(cancel_requested_at_ms(&record))
        .bind(record_json)
        .bind(consumer_hold.as_ref().map(|hold| hold.key.clone()))
        .bind(consumer_hold.as_ref().map(|hold| hold.owner.storage_kind()))
        .bind(consumer_hold.as_ref().map(|hold| hold.owner.storage_id()))
        .bind(consumer_hold.as_ref().map(|hold| hold.cancels))
        .execute(crate::observed_sql::executor(&mut ***tx))
        .await
        .map_err(plugin_sqlx_error)?;
    // On this tier alone the read that found no retained process for the
    // key and the insert that acts on it are two statements in one
    // `READ COMMITTED` transaction, so each takes its own snapshot: two
    // callers presenting one key can both read "no row". The insert orders
    // the pair: the second waits on the first's uncommitted start-key entry
    // until it commits, and `ON CONFLICT DO NOTHING` then reports zero rows
    // instead of raising the start-key unique index. Re-read the winner
    // under the next statement's own snapshot.
    if result.rows_affected() == 0 {
        let winner = match start_key.as_ref() {
            Some(start_key) => load_process_by_start_key_tx(tx, start_key).await?,
            None => None,
        };
        let Some(winner) = winner else {
            return Err(PluginError::Session(format!(
                "process `{}` lost the registration insert race to a row that no longer exists",
                record.id
            )));
        };
        return Ok(AppliedRegistration::LostRace { winner });
    }
    if let Some(env) = record.env_ref.as_ref() {
        crate::artifact_store::acquire_process_env_tx(tx, env, &record.id).await?;
    }
    // The process's actor commits with its row, ready: the start is a wake
    // of the actor, never a relayed obligation (ADR 0132 §12).
    crate::durable::processes::create_actor_within(
        tx,
        &record.id,
        record.input.unstarted_formats().as_str(),
        lash_durable::DurableInstant(i64::try_from(now).unwrap_or(i64::MAX)),
    )
    .await
    .map_err(|error| PluginError::Session(error.to_string()))?;
    let process_id = record.id.clone();
    for session_id in observers {
        sqlx::query(process_sql().observer.insert.sql())
            .bind(session_id.as_str())
            .bind(process_id.as_str())
            .execute(crate::observed_sql::executor(&mut ***tx))
            .await
            .map_err(plugin_sqlx_error)?;
        append_process_event_tx(
            tx,
            &mut record,
            ProcessEventAppendRequest::observer_added(
                &process_id,
                &session_id,
                &ProcessObserverBy::host("registration"),
            ),
            now,
            fleet,
        )
        .await?;
    }
    Ok(AppliedRegistration::Created(record))
}
