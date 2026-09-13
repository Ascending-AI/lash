//! The Postgres schema the workers runbook's own harness tables live in.
//!
//! Split out of `lib.rs`, which sits at the production file-size budget.

use anyhow::{Context, Result};
use sqlx::PgPool;

pub async fn ensure_e2e_schema(pool: &PgPool) -> Result<()> {
    let mut tx = pool.begin().await.context("begin e2e schema transaction")?;
    sqlx::query("SELECT pg_advisory_xact_lock(715421, 907002)")
        .execute(&mut *tx)
        .await
        .context("acquire e2e schema lock")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_worker_events (
            event_id BIGSERIAL PRIMARY KEY,
            workflow_id TEXT NOT NULL,
            worker_id TEXT NOT NULL,
            event_type TEXT NOT NULL,
            detail_json TEXT NOT NULL DEFAULT '{}',
            created_at_ms BIGINT NOT NULL,
            UNIQUE (workflow_id, worker_id, event_type)
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e worker events table")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_terminal_results (
            workflow_id TEXT PRIMARY KEY,
            process_id TEXT NOT NULL,
            worker_id TEXT NOT NULL,
            attachment_id TEXT NOT NULL,
            final_text TEXT NOT NULL,
            submitted_json TEXT NOT NULL DEFAULT '{}',
            queued_turn_ran BOOLEAN NOT NULL DEFAULT FALSE,
            streamed_event_count BIGINT NOT NULL DEFAULT 0,
            replay_cursor TEXT,
            created_at_ms BIGINT NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e terminal results table")?;
    sqlx::query(
        "ALTER TABLE lash_e2e_terminal_results ADD COLUMN IF NOT EXISTS submitted_json TEXT NOT NULL DEFAULT '{}'",
    )
    .execute(&mut *tx)
    .await
    .context("add e2e submitted_json column")?;
    sqlx::query(
        "ALTER TABLE lash_e2e_terminal_results ADD COLUMN IF NOT EXISTS queued_turn_ran BOOLEAN NOT NULL DEFAULT FALSE",
    )
    .execute(&mut *tx)
    .await
    .context("add e2e queued_turn_ran column")?;
    sqlx::query(
        "ALTER TABLE lash_e2e_terminal_results ADD COLUMN IF NOT EXISTS streamed_event_count BIGINT NOT NULL DEFAULT 0",
    )
    .execute(&mut *tx)
    .await
    .context("add e2e streamed_event_count column")?;
    sqlx::query(
        "ALTER TABLE lash_e2e_terminal_results ADD COLUMN IF NOT EXISTS replay_cursor TEXT",
    )
    .execute(&mut *tx)
    .await
    .context("add e2e replay_cursor column")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_failover_markers (
            workflow_id TEXT PRIMARY KEY,
            worker_id TEXT NOT NULL,
            peer_takeover_expected BOOLEAN NOT NULL DEFAULT TRUE,
            created_at_ms BIGINT NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e failover markers table")?;
    sqlx::query(
        "ALTER TABLE lash_e2e_failover_markers \
         ADD COLUMN IF NOT EXISTS peer_takeover_expected BOOLEAN NOT NULL DEFAULT TRUE",
    )
    .execute(&mut *tx)
    .await
    .context("add e2e peer_takeover_expected column")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_harness_signals (
            signal_name TEXT PRIMARY KEY,
            created_at_ms BIGINT NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e harness signals table")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_provider_calls (
            call_id BIGSERIAL PRIMARY KEY,
            request_id TEXT NOT NULL,
            scenario TEXT NOT NULL,
            workflow_id TEXT NOT NULL,
            model TEXT NOT NULL,
            request_json TEXT NOT NULL,
            response_json TEXT NOT NULL,
            created_at_ms BIGINT NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e provider calls table")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_tool_events (
            event_id BIGSERIAL PRIMARY KEY,
            workflow_id TEXT NOT NULL,
            worker_id TEXT NOT NULL,
            tool_name TEXT NOT NULL,
            call_id TEXT,
            args_json TEXT NOT NULL DEFAULT '{}',
            result_json TEXT NOT NULL DEFAULT '{}',
            created_at_ms BIGINT NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e tool events table")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_tool_attempt_counts (
            workflow_id TEXT NOT NULL,
            step_id TEXT NOT NULL,
            count BIGINT NOT NULL,
            last_worker_id TEXT NOT NULL,
            updated_at_ms BIGINT NOT NULL,
            PRIMARY KEY (workflow_id, step_id)
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e tool attempt counts table")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS lash_e2e_turn_events (
            event_id BIGSERIAL PRIMARY KEY,
            workflow_id TEXT NOT NULL,
            worker_id TEXT NOT NULL,
            stream_name TEXT NOT NULL,
            cursor TEXT,
            activity_json TEXT NOT NULL,
            created_at_ms BIGINT NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create e2e turn events table")?;
    tx.commit().await.context("commit e2e schema transaction")?;
    Ok(())
}
