//! Manual real-model acceptance entry point for FIG-1388.

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(slack_clone::live_e2e::run())
}
