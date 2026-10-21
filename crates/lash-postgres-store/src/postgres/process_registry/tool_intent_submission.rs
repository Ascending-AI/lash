use lash_execution_contracts::plugin_error::PluginError;
use lash_execution_contracts::tool_intent::ToolIntentLedgerColumns;
use lash_execution_contracts::tool_intent::ToolIntentSubmissionAdmission;
use lash_execution_contracts::tool_intent::ToolIntentSubmissionRecord;
use lash_execution_contracts::tool_intent_outcome::ToolIntentExecutionOutcome;
use lash_vocabulary::identity::SessionId;
use lash_vocabulary::identity::TurnId;
use sqlx::{PgConnection, PgPool, Row};

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
    let columns = submission.ledger_columns()?;
    let mut tx = crate::begin_guarded(pool, fence)
        .await
        .map_err(crate::plugin_store_error)?;
    crate::observed_sql::query(
        crate::connection_sql::connection_sql()
            .lock_xact_evidence_retention_shared
            .sql(),
    )
    .execute(crate::observed_sql::executor(&mut **tx))
    .await
    .map_err(plugin_sqlx_error)?;
    let sql = crate::turn_ingress::turn_ingress_sql();
    let owner = submission.identity.owner.to_string();
    let retired: bool =
        crate::observed_sql::query_scalar(sql.tool_intents.select_owner_retired.sql())
            .bind(&owner)
            .fetch_one(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(plugin_sqlx_error)?;
    if !retired {
        let (owner_session, owner_run) = columns
            .owning_run
            .as_ref()
            .map(|(session, run)| (session.as_str(), run.as_str()))
            .unzip();
        let inserted = crate::observed_sql::query(sql.tool_intents_postgres.insert_new.sql())
            .bind(&submission.identity.replay_key)
            .bind(&owner)
            .bind(&submission.identity.execution_scope_id)
            .bind(submission.identity.tool_call_id.as_str())
            .bind(i64::from(submission.identity.intent_index))
            .bind(&submission.payload_hash)
            .bind(&columns.envelope_json)
            .bind(columns.intent_json.as_deref())
            .bind(columns.settlement_json.as_deref())
            .bind(owner_session)
            .bind(owner_run)
            .bind(crate::support::clamp_epoch_ms(admitted_at_ms))
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(plugin_sqlx_error)?;
        if inserted.rows_affected() == 1 {
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(ToolIntentSubmissionAdmission::Admitted);
        }
    }
    let admission = match read(&mut tx, &submission.identity.replay_key).await? {
        Some(existing) => ToolIntentSubmissionAdmission::Existing(Box::new(existing)),
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
) -> Result<lash_core_store::store::StoreTransition<ToolIntentSubmissionRecord>, PluginError> {
    let mut tx = crate::begin_guarded(pool, fence)
        .await
        .map_err(crate::plugin_store_error)?;
    let sql = crate::turn_ingress::turn_ingress_sql();
    crate::observed_sql::query(sql.tool_intents_postgres.lock_by_replay_key.sql())
        .bind(replay_key)
        .fetch_one(crate::observed_sql::executor(&mut **tx))
        .await
        .map_err(plugin_sqlx_error)?;
    let mut submission = read(&mut tx, replay_key)
        .await?
        .ok_or_else(|| plugin_sqlx_error(sqlx::Error::RowNotFound))?;
    let changed = submission.complete(at_ms, outcome)?;
    if changed {
        let settlement =
            serde_json::to_string(&submission.settlement).map_err(process_decode_error)?;
        crate::observed_sql::query(sql.tool_intents.settle.sql())
            .bind(replay_key)
            .bind(settlement)
            .execute(crate::observed_sql::executor(&mut **tx))
            .await
            .map_err(plugin_sqlx_error)?;
    }
    tx.commit().await.map_err(plugin_sqlx_error)?;
    Ok(lash_core_store::store::StoreTransition {
        record: submission,
        changed,
    })
}

/// The submission recorded under `replay_key`, its payload expired when its
/// owning run's revision is no longer retained.
async fn read(
    conn: &mut PgConnection,
    replay_key: &str,
) -> Result<Option<ToolIntentSubmissionRecord>, PluginError> {
    let row = crate::observed_sql::query(
        crate::turn_ingress::turn_ingress_sql()
            .tool_intents
            .select_by_replay_key
            .sql(),
    )
    .bind(replay_key)
    .fetch_optional(crate::observed_sql::executor(&mut *conn))
    .await
    .map_err(plugin_sqlx_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let owning_run = match (
        row.get::<Option<String>, _>(3),
        row.get::<Option<String>, _>(4),
    ) {
        (Some(session), Some(run)) => Some((
            SessionId::parse(session).map_err(owning_run_corrupt)?,
            TurnId::parse(run).map_err(owning_run_corrupt)?,
        )),
        _ => None,
    };
    ToolIntentSubmissionRecord::from_ledger_columns(ToolIntentLedgerColumns {
        envelope_json: row.get(0),
        intent_json: row.get(1),
        settlement_json: row.get(2),
        owning_run,
    })
    .map(Some)
}

fn owning_run_corrupt(error: impl std::fmt::Display) -> PluginError {
    PluginError::Session(format!(
        "failed to decode a tool-intent submission's owning run: {error}"
    ))
}
