//! PostgreSQL cold-process recovery helper for durable waits and effect replay.

use std::sync::Arc;

use lash_core::AwaitEventResolver as _;
use lash_core::{AwaitEventWaitIdentity, ExecutionScope};
use lash_postgres_store::{
    PostgresEffectReplayOptions, PostgresRuntimeEffectController, PostgresStorage,
};

#[path = "../../../lash-core/tests/support/cold_process_effect_driver.rs"]
mod cold_process_effect_driver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("LASH_POSTGRES_DATABASE_URL")?;
    let mut args = std::env::args().skip(1);
    let action = args.next().ok_or("missing action or identity")?;
    let nonce = args.next().ok_or("missing vector nonce")?;
    if action.starts_with("effect_") {
        let marker = std::path::PathBuf::from(args.next().ok_or("missing effect marker path")?);
        if args.next().is_some() {
            return Err("unexpected helper arguments".into());
        }
        return run_effect_action(&database_url, &action, &nonce, &marker).await;
    }
    if args.next().is_some() {
        return Err("unexpected helper arguments".into());
    }

    let session_id = format!("cold-process-{nonce}-session");
    let scope = ExecutionScope::turn(&session_id, format!("cold-process-{nonce}-turn"));
    let wait = match action.as_str() {
        "tool_completion" => {
            AwaitEventWaitIdentity::tool_completion(format!("cold-process-{nonce}-call"))
        }
        "turn_cancel_gate" => AwaitEventWaitIdentity::TurnCancelGate,
        other => return Err(format!("unknown identity `{other}`").into()),
    };
    let storage = PostgresStorage::connect(&database_url).await?;
    let host = Arc::new(storage.effect_host());
    let key = host.await_event_key(&scope, wait).await?;
    let waiter_host = Arc::clone(&host);
    let waiter_key = key.clone();
    let waiter = tokio::spawn(async move {
        waiter_host
            .await_await_event(
                &waiter_key,
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let registered: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM lash_await_event_waits WHERE key_id = $1")
                    .bind(&key.key_id)
                    .fetch_one(storage.pool())
                    .await?;
            if registered == 1 {
                break Ok::<(), sqlx::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| "timed out waiting for PostgreSQL await-event registration")??;
    println!("{}", serde_json::to_string(&key)?);
    std::io::Write::flush(&mut std::io::stdout())?;

    waiter.await??;
    Ok(())
}

async fn run_effect_action(
    database_url: &str,
    action: &str,
    nonce: &str,
    marker: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let storage = PostgresStorage::connect(database_url).await?;
    let session_id = format!("cold-process-effect-{nonce}-session");
    let turn_id = format!("cold-process-effect-{nonce}-turn");
    let controller = PostgresRuntimeEffectController::with_options(
        &storage,
        ExecutionScope::turn(&session_id, &turn_id),
        PostgresEffectReplayOptions {
            lease_timings: lash_core::facade_support::LeaseTimings::new(
                cold_process_effect_driver::RECOVERY_TTL,
                cold_process_effect_driver::RECOVERY_RENEW,
            )?,
        },
    );
    if action == "effect_replay" {
        controller.start_replay();
    }
    cold_process_effect_driver::run_effect_action(
        &controller,
        action,
        &session_id,
        &turn_id,
        nonce,
        marker,
    )
    .await
}
