use lash_core::{
    PluginError, ToolIntentExecutionOutcome, ToolIntentSubmissionAdmission,
    ToolIntentSubmissionRecord,
};
use rusqlite::{OptionalExtension, params};

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
                        "INSERT OR IGNORE INTO tool_intent_submissions (
                            replay_key, session_id, execution_scope_id, tool_call_id,
                            intent_index, kind, payload_hash, submission_json
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            replay_key,
                            submission.identity.session_id,
                            submission.identity.execution_scope_id,
                            submission.identity.tool_call_id,
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
                        "SELECT submission_json FROM tool_intent_submissions WHERE replay_key = ?1",
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
                        "SELECT submission_json FROM tool_intent_submissions WHERE replay_key = ?1",
                        params![replay_key],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(process_sqlite_error)?;
                let mut submission: ToolIntentSubmissionRecord =
                    serde_json::from_str(&encoded).map_err(process_decode_error)?;
                if submission.outcome.is_none() {
                    submission.outcome = Some(outcome);
                    tx.execute(
                        "UPDATE tool_intent_submissions SET submission_json = ?2 WHERE replay_key = ?1",
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

pub(super) async fn pending_parent_end(
    registry: &SqliteProcessRegistry,
    session_id: &str,
    execution_scope_id: &str,
) -> Result<Vec<ToolIntentSubmissionRecord>, PluginError> {
    let session_id = session_id.to_string();
    let execution_scope_id = execution_scope_id.to_string();
    let rows = registry
        .conn
        .call(move |conn| {
            let mut statement = conn.prepare(
                "SELECT submission_json FROM tool_intent_submissions
                 WHERE session_id = ?1 AND execution_scope_id = ?2
                 ORDER BY intent_index",
            )?;
            let rows = statement.query_map(params![session_id, execution_scope_id], |row| {
                row.get::<_, String>(0)
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(process_sqlite_error)?;
    rows.into_iter()
        .map(|encoded| {
            serde_json::from_str::<ToolIntentSubmissionRecord>(&encoded)
                .map_err(process_decode_error)
        })
        .filter_map(|decoded| match decoded {
            Ok(submission)
                if !submission.parent_end_settled
                    && matches!(
                        submission.outcome,
                        Some(ToolIntentExecutionOutcome::Executed {
                            parent_end: Some(_),
                            ..
                        })
                    ) =>
            {
                Some(Ok(submission))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

pub(super) async fn complete_parent_end(
    registry: &SqliteProcessRegistry,
    replay_key: &str,
) -> Result<(), PluginError> {
    let replay_key = replay_key.to_string();
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                let encoded = tx
                    .query_row(
                        "SELECT submission_json FROM tool_intent_submissions WHERE replay_key = ?1",
                        params![replay_key],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(process_sqlite_error)?;
                if let Some(encoded) = encoded {
                    let mut submission: ToolIntentSubmissionRecord =
                        serde_json::from_str(&encoded).map_err(process_decode_error)?;
                    if !submission.parent_end_settled {
                        submission.parent_end_settled = true;
                        tx.execute(
                            "UPDATE tool_intent_submissions SET submission_json = ?2 WHERE replay_key = ?1",
                            params![
                                submission.identity.replay_key,
                                serde_json::to_string(&submission)
                                    .map_err(process_decode_error)?,
                            ],
                        )
                        .map_err(process_sqlite_error)?;
                    }
                }
                Ok(())
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}
