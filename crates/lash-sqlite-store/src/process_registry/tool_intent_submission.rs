use lash_core_execution::{
    PluginError, SessionId, ToolIntentExecutionOutcome, ToolIntentLedgerColumns,
    ToolIntentSubmissionAdmission, ToolIntentSubmissionRecord, TurnId,
};
use rusqlite::{OptionalExtension as _, Transaction, params};

use super::{SqliteProcessRegistry, process_decode_error, process_sqlite_error, tx_outcome};

/// Claim the submission's identity, answering its first writer. A fenced
/// owner (FIG-1509) claims nothing new: its reclaimed identities answer
/// [`ToolIntentSubmissionAdmission::Reclaimed`], while a row the lever has not
/// reached yet still answers as the first writer. The process registry has one
/// writer, so the fence the lever installs and this check serialize.
pub(super) async fn admit(
    registry: &SqliteProcessRegistry,
    submission: ToolIntentSubmissionRecord,
) -> Result<ToolIntentSubmissionAdmission, PluginError> {
    let columns = submission.ledger_columns()?;
    let admitted_at_ms = registry.clock.timestamp_ms();
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                let sql = crate::turn_ingress::tool_intent_sql();
                let owner = submission.identity.owner.to_string();
                let retired: bool = tx
                    .query_row(
                        sql.shared.select_owner_retired.sql(),
                        params![owner],
                        |row| row.get(0),
                    )
                    .map_err(process_sqlite_error)?;
                if !retired {
                    let (owner_session, owner_run) = columns
                        .owning_run
                        .as_ref()
                        .map(|(session, run)| (session.as_str(), run.as_str()))
                        .unzip();
                    let inserted = tx
                        .execute(
                            sql.sqlite.insert_new.sql(),
                            params![
                                submission.identity.replay_key,
                                owner,
                                submission.identity.execution_scope_id.as_str(),
                                submission.identity.tool_call_id.as_str(),
                                i64::from(submission.identity.intent_index),
                                submission.payload_hash,
                                columns.envelope_json,
                                columns.intent_json,
                                columns.settlement_json,
                                owner_session,
                                owner_run,
                                crate::clamp_epoch_ms(admitted_at_ms),
                            ],
                        )
                        .map_err(process_sqlite_error)?;
                    if inserted == 1 {
                        return Ok(ToolIntentSubmissionAdmission::Admitted);
                    }
                }
                match read(tx, &submission.identity.replay_key)? {
                    Some(existing) => {
                        Ok(ToolIntentSubmissionAdmission::Existing(Box::new(existing)))
                    }
                    None if retired => Ok(ToolIntentSubmissionAdmission::Reclaimed),
                    None => Err(process_sqlite_error(rusqlite::Error::QueryReturnedNoRows)),
                }
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn complete(
    registry: &SqliteProcessRegistry,
    replay_key: &str,
    outcome: ToolIntentExecutionOutcome,
) -> Result<lash_core_execution::store::StoreTransition<ToolIntentSubmissionRecord>, PluginError> {
    let replay_key = replay_key.to_string();
    let at_ms = registry.clock.timestamp_ms();
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                let mut submission = read(tx, &replay_key)?
                    .ok_or_else(|| process_sqlite_error(rusqlite::Error::QueryReturnedNoRows))?;
                let changed = submission.complete(at_ms, outcome)?;
                if changed {
                    let settlement = serde_json::to_string(&submission.settlement)
                        .map_err(process_decode_error)?;
                    crate::conn::cached_execute(
                        tx,
                        crate::turn_ingress::tool_intent_sql().shared.settle.sql(),
                        params![submission.identity.replay_key, settlement],
                    )
                    .map_err(process_sqlite_error)?;
                }
                Ok(lash_core_execution::store::StoreTransition {
                    record: submission,
                    changed,
                })
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}

/// The submission recorded under `replay_key`, its payload expired when its
/// owning run's revision is no longer retained.
fn read(
    tx: &Transaction<'_>,
    replay_key: &str,
) -> Result<Option<ToolIntentSubmissionRecord>, PluginError> {
    let columns = tx
        .query_row(
            crate::turn_ingress::tool_intent_sql()
                .shared
                .select_by_replay_key
                .sql(),
            params![replay_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(process_sqlite_error)?;
    let Some((envelope_json, intent_json, settlement_json, owner_session, owner_run)) = columns
    else {
        return Ok(None);
    };
    let owning_run = match (owner_session, owner_run) {
        (Some(session), Some(run)) => Some((
            SessionId::parse(session).map_err(owning_run_corrupt)?,
            TurnId::parse(run).map_err(owning_run_corrupt)?,
        )),
        _ => None,
    };
    ToolIntentSubmissionRecord::from_ledger_columns(ToolIntentLedgerColumns {
        envelope_json,
        intent_json,
        settlement_json,
        owning_run,
    })
    .map(Some)
}

fn owning_run_corrupt(error: impl std::fmt::Display) -> PluginError {
    PluginError::Session(format!(
        "failed to decode a tool-intent submission's owning run: {error}"
    ))
}
