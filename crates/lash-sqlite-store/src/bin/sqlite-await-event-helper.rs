//! SQLite cold-process recovery helper for durable waits and effect replay.

use std::path::PathBuf;
use std::sync::Arc;

use lash_core::AwaitEventResolver as _;
use lash_core::{AwaitEventWaitIdentity, ExecutionScope};
use lash_sqlite_store::{
    SqliteEffectHost, SqliteEffectReplayOptions, SqliteRuntimeEffectController,
};

#[path = "../../../lash-core/tests/support/cold_process_effect_driver.rs"]
mod cold_process_effect_driver;
#[path = "../../../lash-core/tests/support/cold_process_turn_driver.rs"]
mod cold_process_turn_driver;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let database = PathBuf::from(args.next().ok_or("missing SQLite database path")?);
    let action = args.next().ok_or("missing action or identity")?;
    let nonce = args.next().ok_or("missing vector nonce")?;
    if matches!(
        action.as_str(),
        "turn_provider_mid_stream"
            | "turn_provider_after_tool_mid_stream"
            | "turn_effect_after_external"
            | "turn_final_commit_boundary"
            | "turn_final_commit_inside"
            | "turn_recover"
    ) {
        let marker = args.next().map(PathBuf::from);
        if args.next().is_some() {
            return Err("unexpected helper arguments".into());
        }
        return run_turn_action(&database, &action, &nonce, marker).await;
    }
    if action.starts_with("effect_") {
        let marker = PathBuf::from(args.next().ok_or("missing effect marker path")?);
        if args.next().is_some() {
            return Err("unexpected helper arguments".into());
        }
        return run_effect_action(&database, &action, &nonce, &marker).await;
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
    let host = Arc::new(SqliteEffectHost::open(&database).await?);
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
            let connection = rusqlite::Connection::open(&database)?;
            let registered: i64 = connection.query_row(
                "SELECT COUNT(*) FROM await_event_waits WHERE key_id = ?1",
                rusqlite::params![key.key_id.as_str()],
                |row| row.get(0),
            )?;
            if registered == 1 {
                break Ok::<(), rusqlite::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| "timed out waiting for SQLite await-event registration")??;
    println!("{}", serde_json::to_string(&key)?);
    std::io::Write::flush(&mut std::io::stdout())?;

    waiter.await??;
    Ok(())
}

async fn run_turn_action(
    database: &std::path::Path,
    action: &str,
    nonce: &str,
    marker: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = Arc::new(lash_sqlite_store::Store::open(database).await?)
        as Arc<dyn lash_core::RuntimePersistence>;
    let effect_database = database.with_extension("effects.db");
    let scope = lash_core::testing::conformance::cold_process_turn_scope(nonce);
    let controller = Arc::new(
        SqliteRuntimeEffectController::open_with_options(
            &effect_database,
            scope,
            SqliteEffectReplayOptions {
                lease_timings: lash_core::facade_support::LeaseTimings::new(
                    cold_process_effect_driver::RECOVERY_TTL,
                    cold_process_effect_driver::RECOVERY_RENEW,
                )?,
            },
        )
        .await?,
    );
    cold_process_turn_driver::run_real_turn_action(store, controller, action, nonce, marker).await
}

async fn run_effect_action(
    database: &std::path::Path,
    action: &str,
    nonce: &str,
    marker: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let session_id = format!("cold-process-effect-{nonce}-session");
    let turn_id = format!("cold-process-effect-{nonce}-turn");
    let scope = ExecutionScope::turn(&session_id, &turn_id);
    let controller = SqliteRuntimeEffectController::open_with_options(
        database,
        scope,
        SqliteEffectReplayOptions {
            lease_timings: lash_core::facade_support::LeaseTimings::new(
                cold_process_effect_driver::RECOVERY_TTL,
                cold_process_effect_driver::RECOVERY_RENEW,
            )?,
        },
    )
    .await?;
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
