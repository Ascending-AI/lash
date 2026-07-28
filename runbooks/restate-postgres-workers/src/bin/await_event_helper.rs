use anyhow::{Context, Result};
use lash_core::AwaitEventResolver as _;
use lash_core::{AwaitEventWaitIdentity, ExecutionScope, IncarnationId};
use lash_restate::RestateEffectHost;
use std::io::Write as _;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let ingress = args.next().context("missing Restate ingress URL")?;
    let identity = args
        .next()
        .context("missing identity: tool_completion or turn_cancel_gate")?;
    let nonce = args.next().context("missing vector nonce")?;
    anyhow::ensure!(args.next().is_none(), "unexpected helper arguments");

    let session_id = format!("cold-process-{nonce}-session");
    let scope = ExecutionScope::turn_incarnation(
        &session_id,
        IncarnationId::decode_from_store(format!("workers-e2e-fixture:{session_id}")),
        format!("cold-process-{nonce}-turn"),
    );
    let wait = match identity.as_str() {
        "tool_completion" => {
            AwaitEventWaitIdentity::tool_completion(format!("cold-process-{nonce}-call"))
        }
        "turn_cancel_gate" => AwaitEventWaitIdentity::TurnCancelGate,
        other => anyhow::bail!("unknown identity `{other}`"),
    };
    let host = RestateEffectHost::new(ingress);
    let key = host
        .await_event_key(&scope, wait)
        .await
        .context("mint helper await-event key")?;
    println!("{}", serde_json::to_string(&key).context("encode key")?);
    std::io::stdout().flush().context("flush key to parent")?;

    let _ = host
        .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
        .await;
    Ok(())
}
