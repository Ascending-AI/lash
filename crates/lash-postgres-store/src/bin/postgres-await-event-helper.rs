use std::io::Write as _;

use lash_core::AwaitEventResolver as _;
use lash_core::{AwaitEventWaitIdentity, ExecutionScope};
use lash_postgres_store::PostgresStorage;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let database_url = std::env::var("LASH_POSTGRES_DATABASE_URL")?;
    let mut args = std::env::args().skip(1);
    let identity = args
        .next()
        .ok_or("missing identity: tool_completion or turn_cancel_gate")?;
    let nonce = args.next().ok_or("missing vector nonce")?;
    if args.next().is_some() {
        return Err("unexpected helper arguments".into());
    }

    let session_id = format!("cold-process-{nonce}-session");
    let scope = ExecutionScope::turn(&session_id, format!("cold-process-{nonce}-turn"));
    let wait = match identity.as_str() {
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
