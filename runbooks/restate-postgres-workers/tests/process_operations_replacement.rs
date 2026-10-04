//! The same replacement phases as the live companion, on SQLite and the Restate double.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use lash::runtime::AdmittedScope;
use lash_restate_postgres_workers_e2e::process_operations::{self, StatePlugin};

#[tokio::test(flavor = "multi_thread")]
async fn start_key_feed_and_plugin_state_survive_worker_replacement() -> Result<()> {
    let double = lash_restate_test::backend(4692, Default::default()).await?;
    let plugin = StatePlugin::default();
    let core = process_operations::core(double.lash_backend(), plugin.clone())?;
    let handler = double
        .open_handler(AdmittedScope::runtime_operation("replacement-prepare"))
        .await
        .map_err(|error| anyhow!(error))?;
    let before = process_operations::prepare(&core, &plugin, handler.scoped()).await?;
    handler.close().await.map_err(|error| anyhow!(error))?;
    double
        .settle_session_shift(&lash::SessionId::from(process_operations::SESSION_ID))
        .await;
    core.shutdown().await?;
    drop(core);

    let double = double.restart().await?;
    let plugin = StatePlugin::default();
    let core = process_operations::core(double.lash_backend(), plugin.clone())?;
    let result = Arc::new(tokio::sync::Mutex::new(None));
    let recovered = Arc::clone(&result);
    let recovery_core = core.clone();
    // The lender's loan counter restarts with the worker, while completed
    // workflow keys survive. Recovery uses the separate replayable handler host.
    double
        .run_in_handler(
            AdmittedScope::runtime_operation("replacement-recover"),
            Arc::new(move |scoped| {
                let core = recovery_core.clone();
                let plugin = plugin.clone();
                let before = before.clone();
                let recovered = Arc::clone(&recovered);
                Box::pin(async move {
                    let outcome =
                        process_operations::recover(&core, &plugin, &before, scoped).await;
                    *recovered.lock().await = Some(outcome);
                })
            }),
        )
        .await
        .map_err(|error| anyhow!(error))?;
    result
        .lock()
        .await
        .take()
        .context("recovery did not run")??;
    core.shutdown().await?;
    Ok(())
}
