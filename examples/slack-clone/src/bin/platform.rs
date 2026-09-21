//! The chat platform: `slack-clone-platform`.
//!
//! Deliberately has no Lash dependency.

use anyhow::Result;
use slack_clone::platform::{self, PlatformConfig};

fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(platform::run(PlatformConfig::from_env()?))
}
