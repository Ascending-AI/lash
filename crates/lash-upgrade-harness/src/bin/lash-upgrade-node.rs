//! `lash-upgrade-node`: one Phase A build of lash as a process (ADR 0115 §6).
//! See [`lash_upgrade_harness::node`].

use clap::Parser;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = lash_upgrade_harness::node::Cli::parse();
    match lash_upgrade_harness::node::run(cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("lash-upgrade-node: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
