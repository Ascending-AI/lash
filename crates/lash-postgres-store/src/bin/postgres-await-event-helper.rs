use std::io::Write as _;

use lash_core::{AwaitEventResolver as _, RuntimeEffectController as _};
use lash_core::{AwaitEventWaitIdentity, ExecutionScope};
use lash_postgres_store::{
    PostgresEffectReplayOptions, PostgresRuntimeEffectController, PostgresStorage,
};

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
    let host = PostgresStorage::connect(&database_url).await?.effect_host();
    let key = host.await_event_key(&scope, wait).await?;
    println!("{}", serde_json::to_string(&key)?);
    std::io::stdout().flush()?;

    std::future::pending::<()>().await;
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
                std::time::Duration::from_millis(300),
                std::time::Duration::from_millis(100),
            )?,
        },
    );
    let envelope = effect_envelope(&session_id, &turn_id, nonce);
    match action {
        "effect_crash" => {
            let marker = marker.to_path_buf();
            controller
                .execute_effect(
                    envelope,
                    lash_core::RuntimeEffectLocalExecutor::testing(move |_| async move {
                        append_effect_marker(&marker, "crashed");
                        println!("effect_executed");
                        std::io::stdout().flush().expect("flush effect marker");
                        std::process::exit(86);
                    }),
                )
                .await?;
        }
        "effect_complete" => {
            let marker = marker.to_path_buf();
            controller
                .execute_effect(
                    envelope,
                    lash_core::RuntimeEffectLocalExecutor::testing(move |_| async move {
                        append_effect_marker(&marker, "completed");
                        Ok(effect_outcome("recorded"))
                    }),
                )
                .await?;
        }
        "effect_replay" => {
            controller.start_replay();
            controller
                .execute_effect(
                    envelope,
                    lash_core::RuntimeEffectLocalExecutor::unavailable(),
                )
                .await?;
        }
        other => return Err(format!("unknown effect action `{other}`").into()),
    }
    println!("ok");
    Ok(())
}

fn effect_envelope(
    session_id: &str,
    turn_id: &str,
    nonce: &str,
) -> lash_core::RuntimeEffectEnvelope {
    let replay_key = format!("cold-process-effect-{nonce}");
    lash_core::RuntimeEffectEnvelope::new(
        lash_core::RuntimeInvocation::effect(
            lash_core::RuntimeScope::for_turn(session_id, turn_id, 1, 0),
            replay_key.clone(),
            lash_core::RuntimeEffectKind::ExecCode,
            replay_key,
        ),
        lash_core::RuntimeEffectCommand::ExecCode {
            language: "conformance".to_string(),
            code: "external-effect".to_string(),
        },
    )
}

fn append_effect_marker(path: &std::path::Path, value: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open external-effect marker");
    writeln!(file, "{value}").expect("append external-effect marker");
    file.flush().expect("flush external-effect marker");
}

fn effect_outcome(marker: &str) -> lash_core::RuntimeEffectOutcome {
    lash_core::RuntimeEffectOutcome::ExecCode {
        result: Box::new(Ok(lash_core::ExecResponse {
            observations: Vec::new(),
            observation_truncation: Vec::new(),
            tool_calls: Vec::new(),
            images: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 0,
            terminal_finish: Some(serde_json::json!(marker)),
        })),
    }
}
