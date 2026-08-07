//! The Lash bot: `slack-clone-bot`.
//!
//! Needs `OPENROUTER_API_KEY` for a real model, and a running
//! `slack-clone-platform` to be a guest inside. It waits for the platform rather
//! than requiring a start order.

use anyhow::Result;
use slack_clone::bot::{self, BotConfig};

/// Lash turns can recurse through plugins and tool calls; the other examples in
/// this repo raise the worker stack for the same reason.
const DEFAULT_TOKIO_THREAD_STACK_BYTES: usize = 8 * 1024 * 1024;

fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let stack_bytes = std::env::var("SLACK_CLONE_BOT_TOKIO_STACK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_TOKIO_THREAD_STACK_BYTES);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(stack_bytes)
        .build()?
        .block_on(bot::run(BotConfig::from_env()?))
}
