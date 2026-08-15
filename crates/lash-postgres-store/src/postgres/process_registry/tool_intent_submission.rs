use lash_core::{
    PluginError, ToolIntentExecutionOutcome, ToolIntentSubmissionAdmission,
    ToolIntentSubmissionRecord,
};
use sqlx::{PgPool, Row};

use crate::{plugin_sqlx_error, process_decode_error};

pub(super) async fn admit(
    pool: &PgPool,
    submission: ToolIntentSubmissionRecord,
) -> Result<ToolIntentSubmissionAdmission, PluginError> {
    let mut tx = pool.begin().await.map_err(plugin_sqlx_error)?;
    let encoded = serde_json::to_string(&submission).map_err(process_decode_error)?;
    let inserted = sqlx::query(
        "INSERT INTO lash_tool_intent_submissions (
            replay_key, session_id, execution_scope_id, tool_call_id,
            intent_index, kind, payload_hash, submission_json
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (replay_key) DO NOTHING",
    )
    .bind(&submission.identity.replay_key)
    .bind(&submission.identity.session_id)
    .bind(&submission.identity.execution_scope_id)
    .bind(&submission.identity.tool_call_id)
    .bind(i64::from(submission.identity.intent_index))
    .bind(submission.kind.as_str())
    .bind(&submission.payload_hash)
    .bind(encoded)
    .execute(&mut *tx)
    .await
    .map_err(plugin_sqlx_error)?;
    if inserted.rows_affected() == 1 {
        tx.commit().await.map_err(plugin_sqlx_error)?;
        return Ok(ToolIntentSubmissionAdmission::Admitted);
    }
    let row = sqlx::query(
        "SELECT submission_json FROM lash_tool_intent_submissions WHERE replay_key = $1",
    )
    .bind(&submission.identity.replay_key)
    .fetch_one(&mut *tx)
    .await
    .map_err(plugin_sqlx_error)?;
    let existing = decode(row.get(0))?;
    tx.commit().await.map_err(plugin_sqlx_error)?;
    Ok(ToolIntentSubmissionAdmission::Existing(Box::new(existing)))
}

pub(super) async fn complete(
    pool: &PgPool,
    replay_key: &str,
    outcome: ToolIntentExecutionOutcome,
) -> Result<ToolIntentSubmissionRecord, PluginError> {
    let mut tx = pool.begin().await.map_err(plugin_sqlx_error)?;
    let row = sqlx::query(
        "SELECT submission_json FROM lash_tool_intent_submissions
         WHERE replay_key = $1 FOR UPDATE",
    )
    .bind(replay_key)
    .fetch_one(&mut *tx)
    .await
    .map_err(plugin_sqlx_error)?;
    let mut submission = decode(row.get(0))?;
    if submission.outcome.is_none() {
        submission.outcome = Some(outcome);
        let encoded = serde_json::to_string(&submission).map_err(process_decode_error)?;
        sqlx::query(
            "UPDATE lash_tool_intent_submissions SET submission_json = $2 WHERE replay_key = $1",
        )
        .bind(replay_key)
        .bind(encoded)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
    }
    tx.commit().await.map_err(plugin_sqlx_error)?;
    Ok(submission)
}

pub(super) async fn pending_parent_end(
    pool: &PgPool,
    session_id: &str,
    execution_scope_id: &str,
) -> Result<Vec<ToolIntentSubmissionRecord>, PluginError> {
    let rows = sqlx::query(
        "SELECT submission_json FROM lash_tool_intent_submissions
         WHERE session_id = $1 AND execution_scope_id = $2
         ORDER BY intent_index",
    )
    .bind(session_id)
    .bind(execution_scope_id)
    .fetch_all(pool)
    .await
    .map_err(plugin_sqlx_error)?;
    rows.into_iter()
        .map(|row| decode(row.get(0)))
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
    pool: &PgPool,
    replay_key: &str,
) -> Result<(), PluginError> {
    let mut tx = pool.begin().await.map_err(plugin_sqlx_error)?;
    let row = sqlx::query(
        "SELECT submission_json FROM lash_tool_intent_submissions
         WHERE replay_key = $1 FOR UPDATE",
    )
    .bind(replay_key)
    .fetch_optional(&mut *tx)
    .await
    .map_err(plugin_sqlx_error)?;
    if let Some(row) = row {
        let mut submission = decode(row.get(0))?;
        if !submission.parent_end_settled {
            submission.parent_end_settled = true;
            let encoded = serde_json::to_string(&submission).map_err(process_decode_error)?;
            sqlx::query(
                "UPDATE lash_tool_intent_submissions SET submission_json = $2 WHERE replay_key = $1",
            )
            .bind(replay_key)
            .bind(encoded)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        }
    }
    tx.commit().await.map_err(plugin_sqlx_error)?;
    Ok(())
}

fn decode(encoded: String) -> Result<ToolIntentSubmissionRecord, PluginError> {
    serde_json::from_str(&encoded).map_err(process_decode_error)
}
