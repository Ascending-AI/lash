//! Writers for the recovery-law witness ledgers (FIG-608).
//!
//! The ledgers live in their own Postgres database under their own account
//! (`witness.sql`, applied by the e2e script), so this module speaks plain SQL
//! over its own pool: nothing here goes through Lash's store set. Every
//! timestamp and digest is computed by that database, and every write error is
//! returned to the caller, which fails its step rather than dropping evidence.

use anyhow::{Context, Result};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::time::{Duration, Instant};

pub const WITNESS_DATABASE_URL_ENV: &str = "WITNESS_DATABASE_URL";

/// Terminal bytes a client read when it first saw the workflow finish.
pub const TERMINAL_OBSERVED: &str = "observed";
/// Terminal bytes the client read from the identical address after a restart.
pub const TERMINAL_REATTACHED: &str = "reattached";

pub const NEMESIS_RESTART_BEGIN: &str = "restart-begin";
pub const NEMESIS_RESTART_COMPLETE: &str = "restart-complete";
pub const NEMESIS_WORKER_EXIT: &str = "worker-exit";
pub const NEMESIS_LOSS_AFTER_COMMIT: &str = "loss-after-commit";

/// The one witnessed tool effect.
pub const WITNESSED_EFFECT_TOOL: &str = "batch_side_effect";

/// The logical identity of one witnessed effect: stable across every physical
/// retry, because it names only the parent workflow, the tool and the key the
/// caller asked for.
pub fn effect_logical_key(parent_workflow_id: &str, tool: &str, key: &str) -> String {
    format!("{parent_workflow_id}/{tool}/{key}")
}

/// Connect to the witness database, retrying while Postgres comes up.
pub async fn connect_witness() -> Result<PgPool> {
    let url = std::env::var(WITNESS_DATABASE_URL_ENV)
        .with_context(|| format!("{WITNESS_DATABASE_URL_ENV} must be set"))?;
    let max_connections = std::env::var("WITNESS_CONNECTIONS_PER_PROCESS")
        .context("WITNESS_CONNECTIONS_PER_PROCESS must declare the witness pool budget")?
        .parse::<std::num::NonZeroU32>()
        .context("parse positive witness connection limit")?
        .get();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(&url)
            .await
        {
            Ok(pool) => return Ok(pool),
            Err(err) if Instant::now() < deadline => {
                tracing::warn!(error = %err, "witness database not ready yet");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(err) => return Err(err).context("connect the witness database"),
        }
    }
}

pub async fn record_submission(pool: &PgPool, workflow_id: &str, request: &[u8]) -> Result<()> {
    sqlx::query("INSERT INTO witness_submissions (workflow_id, request_bytes) VALUES ($1, $2)")
        .bind(workflow_id)
        .bind(request)
        .execute(pool)
        .await
        .with_context(|| format!("witness the submission of `{workflow_id}`"))?;
    Ok(())
}

pub async fn record_acknowledgement(
    pool: &PgPool,
    workflow_id: &str,
    invocation_id: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO witness_acknowledgements (workflow_id, invocation_id) VALUES ($1, $2)",
    )
    .bind(workflow_id)
    .bind(invocation_id)
    .execute(pool)
    .await
    .with_context(|| format!("witness the acknowledgement of `{workflow_id}`"))?;
    Ok(())
}

pub async fn record_client_terminal(
    pool: &PgPool,
    workflow_id: &str,
    phase: &str,
    output: &[u8],
) -> Result<()> {
    sqlx::query(
        "INSERT INTO witness_client_terminals (workflow_id, phase, output_bytes)
         VALUES ($1, $2, $3)",
    )
    .bind(workflow_id)
    .bind(phase)
    .bind(output)
    .execute(pool)
    .await
    .with_context(|| format!("witness the {phase} terminal of `{workflow_id}`"))?;
    Ok(())
}

pub async fn record_nemesis(pool: &PgPool, kind: &str, subject: &str) -> Result<()> {
    sqlx::query("INSERT INTO witness_nemesis (kind, subject) VALUES ($1, $2)")
        .bind(kind)
        .bind(subject)
        .execute(pool)
        .await
        .with_context(|| format!("witness nemesis `{kind}` for `{subject}`"))?;
    Ok(())
}

pub async fn record_provider_receipt(
    pool: &PgPool,
    request_id: &str,
    scenario: &str,
    workflow_id: &str,
    model: &str,
    request: &serde_json::Value,
    response: &serde_json::Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO witness_provider_receipts (
             request_id, scenario, workflow_id, model, request_bytes, response_bytes
         )
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(request_id)
    .bind(scenario)
    .bind(workflow_id)
    .bind(model)
    .bind(request.to_string().into_bytes())
    .bind(response.to_string().into_bytes())
    .execute(pool)
    .await
    .with_context(|| format!("witness provider receipt `{request_id}` for `{workflow_id}`"))?;
    Ok(())
}

/// One physical attempt at a witnessed effect.
pub struct EffectAttempt<'a> {
    pub attempt_id: &'a str,
    pub logical_key: &'a str,
    pub parent_workflow_id: &'a str,
    pub call_id: &'a str,
    pub worker_id: &'a str,
    pub request: &'a [u8],
}

/// Append the attempt, then offer its commit to the idempotent receiver. The
/// first attempt to arrive for a logical key is accepted; every attempt gets
/// back the accepted commit's response, so a retry observes the original
/// effect rather than a new one. Returns whether this attempt was accepted and
/// the committed response.
pub async fn commit_effect(
    pool: &PgPool,
    attempt: &EffectAttempt<'_>,
    response: &[u8],
) -> Result<(bool, Vec<u8>)> {
    sqlx::query(
        "INSERT INTO witness_effect_attempts (
             attempt_id, logical_key, parent_workflow_id, call_id, worker_id, request_bytes
         )
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(attempt.attempt_id)
    .bind(attempt.logical_key)
    .bind(attempt.parent_workflow_id)
    .bind(attempt.call_id)
    .bind(attempt.worker_id)
    .bind(attempt.request)
    .execute(pool)
    .await
    .with_context(|| format!("witness attempt `{}`", attempt.attempt_id))?;
    let accepted = sqlx::query(
        "INSERT INTO witness_effect_commits (
             logical_key, parent_workflow_id, first_attempt_id, request_bytes, response_bytes
         )
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (logical_key) DO NOTHING",
    )
    .bind(attempt.logical_key)
    .bind(attempt.parent_workflow_id)
    .bind(attempt.attempt_id)
    .bind(attempt.request)
    .bind(response)
    .execute(pool)
    .await
    .with_context(|| format!("offer the commit of `{}`", attempt.logical_key))?
    .rows_affected()
        == 1;
    let committed: Vec<u8> = sqlx::query_scalar(
        "SELECT response_bytes FROM witness_effect_commits WHERE logical_key = $1",
    )
    .bind(attempt.logical_key)
    .fetch_one(pool)
    .await
    .with_context(|| format!("read the commit of `{}`", attempt.logical_key))?;
    Ok((accepted, committed))
}

/// Append the receiver's answer to `attempt_id`, once the caller has it.
pub async fn record_effect_reply(
    pool: &PgPool,
    attempt_id: &str,
    logical_key: &str,
    accepted: bool,
    response: &[u8],
) -> Result<()> {
    sqlx::query(
        "INSERT INTO witness_effect_replies (attempt_id, logical_key, accepted, response_bytes)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(attempt_id)
    .bind(logical_key)
    .bind(accepted)
    .bind(response)
    .execute(pool)
    .await
    .with_context(|| format!("witness the reply to attempt `{attempt_id}`"))?;
    Ok(())
}

#[cfg(test)]
mod connection_budget_tests {
    use super::*;
    use sqlx::Connection;

    #[tokio::test]
    #[ignore = "requires PostgreSQL witness database and WITNESS_CONNECTIONS_PER_PROCESS=2"]
    async fn configured_witness_pool_counts_connections_under_load() {
        let url = std::env::var(WITNESS_DATABASE_URL_ENV)
            .ok()
            .filter(|url| !url.trim().is_empty())
            .expect("PostgreSQL witness test requires a non-empty WITNESS_DATABASE_URL");
        assert_eq!(
            std::env::var("WITNESS_CONNECTIONS_PER_PROCESS").as_deref(),
            Ok("2")
        );
        let pool = connect_witness()
            .await
            .expect("connect configured witness pool");
        let first = pool.acquire().await.expect("first connection");
        let second = pool.acquire().await.expect("second connection");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), pool.acquire())
                .await
                .is_err(),
            "configured witness limit was ignored: a third connection opened"
        );
        let mut observer = sqlx::PgConnection::connect(&url).await.expect("observer");
        let observer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut observer)
            .await
            .expect("observer pid");
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let pool = pool.clone();
            tasks.spawn(async move {
                sqlx::query("SELECT pg_sleep(0.005)")
                    .execute(&pool)
                    .await
                    .expect("queued witness work");
            });
        }
        let connections: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE application_name = current_setting('application_name') AND pid <> $1")
            .bind(observer_pid).fetch_one(&mut observer).await.expect("count connections");
        assert_eq!(connections, 2);
        drop((first, second));
        while let Some(task) = tasks.join_next().await {
            task.expect("witness task");
        }
        assert!(pool.size() <= 2);
        println!("witness connections=2 concurrent requests=32 configured cap=2");
        pool.close().await;
    }
    #[test]
    fn postgres_variants_never_pass_without_a_database_url() {
        let executable = std::env::current_exe().expect("test executable");
        let law = "witness::connection_budget_tests::configured_witness_pool_counts_connections_under_load";
        for url in [None, Some(""), Some(" \t ")] {
            let mut command = std::process::Command::new(&executable);
            command
                .args(["--exact", law, "--include-ignored", "--nocapture"])
                .env_remove("WITNESS_DATABASE_URL");
            if let Some(url) = url {
                command.env("WITNESS_DATABASE_URL", url);
            }
            let output = command.output().expect("run PostgreSQL variant");
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stdout.contains("running 1 test"), "{stdout}\n{stderr}");
            assert!(
                !output.status.success() && stdout.contains("0 passed; 1 failed"),
                "{law} with URL {url:?} passed vacuously: {stdout}\n{stderr}"
            );
            assert!(
                stderr.contains("WITNESS_DATABASE_URL"),
                "{stdout}\n{stderr}"
            );
        }
    }
}
