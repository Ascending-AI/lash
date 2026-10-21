use std::net::SocketAddr;

use anyhow::{Context, Result};
use std::sync::Arc;
use workflow_graph_roundtrip::{AppState, workflow_core};

#[tokio::main]
async fn main() -> Result<()> {
    let addr: SocketAddr = std::env::var("WORKFLOW_GRAPH_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3031".to_string())
        .parse()
        .context("invalid WORKFLOW_GRAPH_ADDR")?;
    let database = std::env::var("WORKFLOW_GRAPH_SQLITE_PATH")
        .unwrap_or_else(|_| ".workflow-graph/lash.db".into());
    let stores =
        lash::sqlite::SqliteStoreSet::open(database, lash::sqlite::SqliteSynchronous::Normal)
            .await
            .context("open workflow SQLite stores")?;
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores)).config(lash::durable::DurableSettings { owner_retention: Some(lash::durable::OwnerRetentionSettings { idle_evict: std::time::Duration::from_secs(60),
 max_owner_bytes: const { std::num::NonZeroU64::new(64 * 1024 * 1024).unwrap() },
 max_node_bytes: const { std::num::NonZeroU64::new(512 * 1024 * 1024).unwrap() },
 max_owner_dirty_bytes: const { std::num::NonZeroU64::new(32 * 1024 * 1024).unwrap() },
 max_node_dirty_bytes: const { std::num::NonZeroU64::new(256 * 1024 * 1024).unwrap() }, }), ..lash::durable::DurableSettings::standard() })
        .build()
        .context("build the durable backend")?;
    let core = workflow_core(backend)?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("bind workflow graph listener")?;
    println!("workflow-graph-roundtrip listening on http://{addr}");
    workflow_graph_roundtrip::serve(listener, AppState::new(core).await?)
        .await
        .context("serve workflow graph backend")
}
