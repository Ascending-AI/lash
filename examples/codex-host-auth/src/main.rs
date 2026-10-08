//! `codex-host-auth login <file>`: sign in to ChatGPT and store the login
//! that `CodexHostAuth::open(<file>)` then serves to lash.

use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(command), Some(path)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: codex-host-auth login <file>");
    };
    anyhow::ensure!(command == "login", "usage: codex-host-auth login <file>");
    let path = PathBuf::from(path);
    codex_host_auth::login(&path, |code| {
        println!("Open {} and enter {}", code.verify_url, code.user_code);
    })
    .await?;
    println!("Stored the ChatGPT login at {}", path.display());
    Ok(())
}
