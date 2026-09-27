//! The cross-worker child: `lash-perf latency-worker`.
//!
//! Serves lash's Restate services over the shared store directory on a
//! process of its own, so cross-worker cases measure the real split: the
//! host's sends cross the server, the drive runs here, and the host's
//! follower has neither live replay nor the settled-root mailbox. The parent
//! exports `RESTATE_INGRESS_URL`/`RESTATE_ADMIN_URL` and the run's authority
//! seed, picks the endpoint bind, and polls the ready file.

use std::sync::Arc;

use anyhow::{Context, Result};

use super::LatencyWorkerArgs;
use super::provider::{LatencyProviderKind, ProviderTiming, latency_provider};
use super::restate::{LocalRestate, register_deployment};

/// Serve lash's Restate services for the latency run and park until killed.
pub(crate) async fn run(args: LatencyWorkerArgs) -> Result<()> {
    let ingress_url = std::env::var("RESTATE_INGRESS_URL")
        .context("RESTATE_INGRESS_URL is unset: the parent exports it")?;
    let admin_url = std::env::var("RESTATE_ADMIN_URL")
        .context("RESTATE_ADMIN_URL is unset: the parent exports it")?;
    let authority_seed = std::env::var("LASH_LATENCY_AUTHORITY_SEED")
        .context("LASH_LATENCY_AUTHORITY_SEED is unset: the parent exports it")?;
    let restate = LocalRestate {
        ingress_url,
        admin_url,
        authority: lash::restate::RestateAuthorityId::new(&authority_seed)
            .map_err(|error| anyhow::anyhow!("authority id: {error}"))?,
        source: "worker",
    };
    let stores = lash::sqlite::SqliteStoreSet::open(&args.store_dir)
        .await
        .map_err(|error| anyhow::anyhow!("open worker store set: {error}"))?;
    let engine = restate.engine(Arc::new(stores));
    let backend = lash::Backend::new(engine.clone());
    let effect_host = backend.effect_host();
    let timing = Arc::new(ProviderTiming::default());
    let provider = latency_provider(LatencyProviderKind::Text, timing, None).into_handle();
    let mut plugins = lash::PluginStack::new();
    plugins.push(Arc::new(lash::plugins::StaticPluginFactory::new(
        "latency_tools",
        lash::plugins::PluginSpec::new().with_tool_provider(Arc::new(
            crate::runtime_perf::providers::BenchmarkEchoTool::new(Arc::clone(&effect_host)),
        )),
    )));
    let core = lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("latency-model")
                .context_window_tokens(200_000)
                .build()
                .map_err(|error| anyhow::anyhow!("latency model spec: {error}"))?,
        )
        .plugins(plugins)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "lash-perf-latency-worker",
            format!("{}", std::process::id()),
        ))?;
    let worker = lash::durability::DurableProcessWorker::new(
        core.durable_process_worker_config()
            .context("worker process worker config")?,
    )
    .map_err(|error| anyhow::anyhow!("build the worker process worker: {error}"))?;
    let endpoint = engine.endpoint_builder(worker).build();
    let listener = tokio::net::TcpListener::bind(args.endpoint_bind)
        .await
        .with_context(|| format!("bind the latency worker endpoint at {}", args.endpoint_bind))?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = restate_sdk::http_server::HttpServer::new(endpoint)
            .serve(listener)
            .await;
    });
    register_deployment(&restate.admin_url, &format!("http://{addr}")).await?;
    std::fs::write(&args.ready_file, addr.to_string())
        .with_context(|| format!("write {}", args.ready_file.display()))?;
    // Serve until the parent kills the process.
    std::future::pending::<()>().await;
    Ok(())
}
