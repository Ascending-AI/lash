use lash_core_execution::{
    PluginError, ToolIntentExecutionOutcome, ToolIntentSubmissionAdmission,
    ToolIntentSubmissionRecord,
};
use rusqlite::params;

use super::{SqliteProcessRegistry, process_decode_error, process_sqlite_error, tx_outcome};

pub(super) async fn admit(
    registry: &SqliteProcessRegistry,
    submission: ToolIntentSubmissionRecord,
) -> Result<ToolIntentSubmissionAdmission, PluginError> {
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                let replay_key = submission.identity.replay_key.clone();
                let inserted = tx
                    .execute(
                        crate::turn_ingress::tool_intent_sql()
                            .sqlite
                            .insert_new
                            .sql(),
                        params![
                            replay_key,
                            submission.identity.session_id.as_str(),
                            submission.identity.execution_scope_id.as_str(),
                            submission.identity.tool_call_id.as_str(),
                            i64::from(submission.identity.intent_index),
                            submission.kind.as_str(),
                            submission.payload_hash,
                            serde_json::to_string(&submission).map_err(process_decode_error)?,
                        ],
                    )
                    .map_err(process_sqlite_error)?;
                if inserted == 1 {
                    return Ok(ToolIntentSubmissionAdmission::Admitted);
                }
                let encoded = tx
                    .query_row(
                        crate::turn_ingress::tool_intent_sql()
                            .shared
                            .select_by_replay_key
                            .sql(),
                        params![submission.identity.replay_key],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(process_sqlite_error)?;
                let existing = serde_json::from_str(&encoded).map_err(process_decode_error)?;
                Ok(ToolIntentSubmissionAdmission::Existing(Box::new(existing)))
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn complete(
    registry: &SqliteProcessRegistry,
    replay_key: &str,
    outcome: ToolIntentExecutionOutcome,
) -> Result<ToolIntentSubmissionRecord, PluginError> {
    let replay_key = replay_key.to_string();
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
                if submission.outcome.is_none() {
                    submission.outcome = Some(outcome);
                    tx.execute(
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
                Ok(submission)
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}
