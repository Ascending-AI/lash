//! The chat platform: `slack-clone-platform`.
//!
//! Deliberately has no Lash dependency. Run it, open the printed URL in two
//! browser tabs, and you have two people in a workspace.

use anyhow::Result;
use slack_clone::platform::{self, PlatformConfig};

fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(platform::run(PlatformConfig::from_env()?))
}
