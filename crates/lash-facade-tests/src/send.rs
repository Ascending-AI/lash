use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

/// The text the scripted provider holds its answer on until released.
pub const HELD: &str = "held until released";

/// A provider that answers `echo: <last user text>`, holding the answer to
/// [`HELD`] until `release` is notified, counting its calls.
pub fn scripted_provider(release: Arc<Notify>, calls: Arc<AtomicUsize>) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("send-handle-laws")
        .complete(move |request| {
            let release = Arc::clone(&release);
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let text = last_user_text(&request);
                if text.contains(HELD) {
                    release.notified().await;
                }
                Ok(text_response(&format!("echo: {text}")))
            }
        })
        .build()
        .into_handle()
}

/// A core serving its own node over a fresh SQLite memory store set.
pub struct Fixture {
    pub core: LashCore,
    pub release: Arc<Notify>,
    pub calls: Arc<AtomicUsize>,
}

pub async fn fixture(batch: usize) -> Result<Fixture> {
    fixture_over(batch, |backend| backend).await
}

/// [`fixture`] whose drain takes every eligible input into one run
/// (`DrainMode::All`), for the laws about an input another input's run
/// answers: the default drain gives each input its own run (FIG-4457).
pub async fn composing_fixture() -> Result<Fixture> {
    fixture_over_with_batching(
        lash::QueuedWorkBatchingConfig::new(4).with_drain_mode(lash::DrainMode::All),
        |backend| backend,
    )
    .await
}

/// [`fixture`] over the store set's backend as `layer` rebuilds it.
pub async fn fixture_over(
    batch: usize,
    layer: impl FnOnce(
        lash_execution_contracts::backend::Backend,
    ) -> lash_execution_contracts::backend::Backend,
) -> Result<Fixture> {
    fixture_over_with_batching(lash::QueuedWorkBatchingConfig::new(batch), layer).await
}

pub async fn fixture_over_with_batching(
    batching: lash::QueuedWorkBatchingConfig,
    layer: impl FnOnce(
        lash_execution_contracts::backend::Backend,
    ) -> lash_execution_contracts::backend::Backend,
) -> Result<Fixture> {
    let backend = layer(sqlite_memory_store_backend().await);
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let core = LashCore::standard_builder(backend)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(batching)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(
            scripted_provider(Arc::clone(&release), Arc::clone(&calls)),
            mock_llm_profile_spec(),
        )
        .build(super::runtime_lease_owner())?;
    Ok(Fixture {
        core,
        release,
        calls,
    })
}

/// Wait until the scripted provider has been called `count` times.
pub async fn provider_called(fixture: &Fixture, count: usize) {
    reaches(&fixture.calls, count, "the provider is called").await;
}

/// Wait until `counter` reaches `count`.
pub async fn reaches(counter: &AtomicUsize, count: usize, what: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while counter.load(Ordering::SeqCst) < count {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect(what);
}

/// Wraps the deployment store a fixture's catalog hands out.
pub type StoreMap = Arc<
    dyn Fn(
            Arc<dyn lash_execution_contracts::deployment::DeploymentStore>,
        ) -> Arc<dyn lash_execution_contracts::deployment::DeploymentStore>
        + Send
        + Sync,
>;

/// [`fixture_over`] with the deployment store `map` over the double's.
pub async fn fixture_with_stores(batch: usize, map: StoreMap) -> Result<Fixture> {
    fixture_over(batch, |backend| {
        lash_core::testing::runtime_helpers::LayeredBackend::over(backend)
            .map_session_store_factory(|inner| map(inner))
            .into_backend()
    })
    .await
}
