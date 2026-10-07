//! `lash-postgres-workers-node`: one lash node of the runbook. Its
//! configuration is its environment (`lash_postgres_workers::node::NodeConfig`);
//! its commands arrive on stdin and its reports leave on stdout.

use lash_postgres_workers::node::{NodeConfig, run};

#[tokio::main]
async fn main() {
    let code = match NodeConfig::from_env(|name| std::env::var(name).ok()) {
        Ok(config) => match run(config).await {
            Ok(_) => 0,
            Err(error) => {
                eprintln!("lash-postgres-workers-node: {error}");
                1
            }
        },
        Err(error) => {
            eprintln!("lash-postgres-workers-node: {error}");
            2
        }
    };
    // Exit at once: the command reader may still be blocked on stdin, and a
    // runtime shut down normally would wait for it.
    std::process::exit(code);
}
