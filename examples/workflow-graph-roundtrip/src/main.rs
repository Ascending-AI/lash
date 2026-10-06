use std::net::SocketAddr;

use anyhow::{Context, Result};
use std::sync::Arc;
use workflow_graph_roundtrip::{AppState, workflow_core};
#[path = "../../shared/local_restate.rs"]
mod local_restate;

#[tokio::main]
async fn main() -> Result<()> {
    let addr: SocketAddr = std::env::var("WORKFLOW_GRAPH_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3031".to_string())
        .parse()
        .context("invalid WORKFLOW_GRAPH_ADDR")?;
    let server = local_restate::LocalRestateServer::shared("workflow-graph").await?;
    let restate = server.core("workflow-graph")?;
    let database = std::env::var("WORKFLOW_GRAPH_SQLITE_PATH")
        .unwrap_or_else(|_| ".workflow-graph/lash.db".into());
    let stores = lash::sqlite::SqliteStoreSet::open(database)
        .await
        .context("open workflow SQLite stores")?;
    let engine = restate.engine(Arc::new(stores));
    let core = workflow_core(lash::Backend::new(engine.clone()))?;
    let worker =
        lash::durability::DurableProcessWorker::new(core.durable_process_worker_config()?)?;
    let _deployment = restate
        .serve(
            &engine,
            workflow_graph_roundtrip::bind_commands(
                engine.endpoint_builder(worker)?,
                core.clone(),
                &engine,
            )
            .build(),
        )
        .await?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("bind workflow graph listener")?;
    println!("workflow-graph-roundtrip listening on http://{addr}");
    workflow_graph_roundtrip::serve(
        listener,
        AppState::new(
            core,
            lash::restate::RestateConnection::new(restate.ingress_url.clone()),
        )?,
    )
    .await
    .context("serve workflow graph backend")
}
