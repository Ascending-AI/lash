use lash_core_execution::{
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
        crate::turn_ingress::turn_ingress_sql()
            .tool_intents_postgres
            .insert_new
            .sql(),
    )
    .bind(&submission.identity.replay_key)
    .bind(submission.identity.session_id.as_str())
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
        crate::turn_ingress::turn_ingress_sql()
            .tool_intents
            .select_by_replay_key
            .sql(),
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
        crate::turn_ingress::turn_ingress_sql()
            .tool_intents_postgres
            .select_by_replay_key_for_update
            .sql(),
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
            crate::turn_ingress::turn_ingress_sql()
                .tool_intents
                .update_submission
                .sql(),
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

fn decode(encoded: String) -> Result<ToolIntentSubmissionRecord, PluginError> {
    serde_json::from_str(&encoded).map_err(process_decode_error)
}
