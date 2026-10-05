use lash_core_execution::{
    PluginError, ToolIntentExecutionOutcome, ToolIntentSubmissionAdmission,
    ToolIntentSubmissionRecord,
};
use rusqlite::{OptionalExtension as _, params};

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
    submission.validate_settlement()?;
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
                                serde_json::to_string(&submission).map_err(process_decode_error)?,
                                crate::clamp_epoch_ms(admitted_at_ms),
                            ],
                        )
                        .map_err(process_sqlite_error)?;
                    if inserted == 1 {
                        return Ok(ToolIntentSubmissionAdmission::Admitted);
                    }
                }
                let encoded = tx
                    .query_row(
                        sql.shared.select_by_replay_key.sql(),
                        params![submission.identity.replay_key],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(process_sqlite_error)?;
                match encoded {
                    Some(encoded) => {
                        let existing =
                            serde_json::from_str(&encoded).map_err(process_decode_error)?;
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
                let encoded = tx
                    .query_row(
                        crate::turn_ingress::tool_intent_sql()
                            .shared
                            .select_by_replay_key
                            .sql(),
                        params![replay_key],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(process_sqlite_error)?;
                let mut submission: ToolIntentSubmissionRecord =
                    serde_json::from_str(&encoded).map_err(process_decode_error)?;
                let changed = submission.complete(at_ms, outcome)?;
                if changed {
                    crate::conn::cached_execute(
                        tx,
                        crate::turn_ingress::tool_intent_sql()
                            .shared
                            .update_submission
                            .sql(),
                        params![
                            submission.identity.replay_key,
                            serde_json::to_string(&submission).map_err(process_decode_error)?,
                        ],
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
