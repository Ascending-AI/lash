//! Prebuilt out-of-process V7 publication holder; no engine or journal.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = std::env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: lash-e2e-proxy CONFIG.json"))?;
    let config = serde_json::from_slice(&std::fs::read(config)?)?;
    lash_upgrade_harness::e2e::control::process::run(config).await
}
