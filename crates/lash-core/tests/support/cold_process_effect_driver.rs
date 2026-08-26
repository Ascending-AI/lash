//! Shared effect driver for SQLite/PostgreSQL cold-process recovery helpers.

use std::io::Write as _;

pub const RECOVERY_TTL: std::time::Duration = std::time::Duration::from_millis(300);
pub const RECOVERY_RENEW: std::time::Duration = std::time::Duration::from_millis(100);

pub async fn run_effect_action<C>(
    controller: &C,
    action: &str,
    session_id: &str,
    turn_id: &str,
    nonce: &str,
    marker: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>>
where
    C: lash_core::RuntimeEffectController + ?Sized,
{
    let envelope = effect_envelope(session_id, turn_id, nonce);
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
            tool_calls: Vec::new(),
            executed_calls: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 0,
            degraded_bindings: Vec::new(),
            terminal_finish: Some(serde_json::json!(marker)),
        })),
    }
}
