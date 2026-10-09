use lash_core_execution::{
    PluginError, ToolIntentExecutionOutcome, ToolIntentSubmissionAdmission,
    ToolIntentSubmissionRecord,
};
use sqlx::{PgPool, Row};

use crate::{plugin_sqlx_error, process_decode_error};

/// Claim the submission's identity, answering its first writer. A fenced
/// owner (FIG-1509) claims nothing new: its reclaimed identities answer
/// [`ToolIntentSubmissionAdmission::Reclaimed`], while a row the lever has not
/// reached yet still answers as the first writer. The claim holds the
/// evidence-retention key shared, so a sweep either reclaims after it commits
/// or installs the fence this claim then reads.
pub(super) async fn admit(
    pool: &PgPool,
    fence: &crate::guarded_tx::WriterFence,
    submission: ToolIntentSubmissionRecord,
    admitted_at_ms: u64,
) -> Result<ToolIntentSubmissionAdmission, PluginError> {
    submission.validate_settlement()?;
    let mut tx = crate::begin_guarded(pool, fence)
        .await
        .map_err(crate::plugin_store_error)?;
    sqlx::query(
        crate::connection_sql::connection_sql()
            .lock_xact_evidence_retention_shared
            .sql(),
    )
    .execute(crate::observed_sql::executor(&mut **tx))
    .await
    .map_err(plugin_sqlx_error)?;
    let sql = crate::turn_ingress::turn_ingress_sql();
    let owner = submission.identity.owner.to_string();
    let retired: bool = sqlx::query_scalar(sql.tool_intents.select_owner_retired.sql())
        .bind(&owner)
        .fetch_one(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(plugin_sqlx_error)?;
    if !retired {
        let encoded = serde_json::to_string(&submission).map_err(process_decode_error)?;
        let inserted = sqlx::query(sql.tool_intents_postgres.insert_new.sql())
            .bind(&submission.identity.replay_key)
            .bind(&owner)
            .bind(&submission.identity.execution_scope_id)
            .bind(submission.identity.tool_call_id.as_str())
            .bind(i64::from(submission.identity.intent_index))
            .bind(&submission.payload_hash)
            .bind(encoded)
            .bind(crate::support::clamp_epoch_ms(admitted_at_ms))
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(plugin_sqlx_error)?;
        if inserted.rows_affected() == 1 {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(ToolIntentSubmissionAdmission::Admitted);
        }
    }
    let row = sqlx::query(sql.tool_intents.select_by_replay_key.sql())
        .bind(&submission.identity.replay_key)
        .fetch_optional(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(plugin_sqlx_error)?;
    let admission = match row {
        Some(row) => ToolIntentSubmissionAdmission::Existing(Box::new(decode(row.get(0))?)),
        None if retired => ToolIntentSubmissionAdmission::Reclaimed,
        None => return Err(plugin_sqlx_error(sqlx::Error::RowNotFound)),
    };
    tx.commit().await.map_err(plugin_sqlx_error)?;
    Ok(admission)
}

pub(super) async fn complete(
    pool: &PgPool,
    fence: &crate::guarded_tx::WriterFence,
    replay_key: &str,
    outcome: ToolIntentExecutionOutcome,
    at_ms: u64,
) -> Result<lash_core_execution::store::StoreTransition<ToolIntentSubmissionRecord>, PluginError> {
    let mut tx = crate::begin_guarded(pool, fence)
        .await
        .map_err(crate::plugin_store_error)?;
    let row = sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .tool_intents_postgres
            .select_by_replay_key_for_update
            .sql(),
    )
    .bind(replay_key)
    .fetch_one(crate::observed_sql::executor(&mut **tx))
    .await
    .map_err(plugin_sqlx_error)?;
    let mut submission = decode(row.get(0))?;
    let changed = submission.complete(at_ms, outcome)?;
    if changed {
        let encoded = serde_json::to_string(&submission).map_err(process_decode_error)?;
        sqlx::query(
            crate::turn_ingress::turn_ingress_sql()
                .tool_intents
                .update_submission
                .sql(),
        )
        .bind(replay_key)
        .bind(encoded)
        .execute(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(plugin_sqlx_error)?;
    }
    tx.commit().await.map_err(plugin_sqlx_error)?;
    Ok(lash_core_execution::store::StoreTransition {
        record: submission,
        changed,
    })
}

fn decode(encoded: String) -> Result<ToolIntentSubmissionRecord, PluginError> {
    serde_json::from_str(&encoded).map_err(process_decode_error)
}
