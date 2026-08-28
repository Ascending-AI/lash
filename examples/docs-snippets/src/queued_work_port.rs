//! Worked example for supplying a caller-owned queued-work port.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use lash::provider::ProviderHandle;
use lash::runtime::{
    NativeQueuedWork, QueuedWorkRunError, QueuedWorkRunHandle, QueuedWorkRunProgress,
    QueuedWorkRunRequest, QueuedWorkSubstrate,
};

#[derive(Default)]
struct QueuedWorkMetrics {
    durable_peeks: AtomicUsize,
    drive_passes: AtomicUsize,
}

struct InstrumentedQueuedWorkRunHandle {
    inner: Arc<dyn QueuedWorkRunHandle>,
    metrics: Arc<QueuedWorkMetrics>,
}

#[async_trait]
impl QueuedWorkRunHandle for InstrumentedQueuedWorkRunHandle {
    async fn peek_claimable_queued_work(
        &self,
        session_id: Option<&str>,
    ) -> Result<Option<bool>, QueuedWorkRunError> {
        self.metrics.durable_peeks.fetch_add(1, Ordering::Relaxed);
        self.inner.peek_claimable_queued_work(session_id).await
    }

    async fn run_queued_work(
        &self,
        request: QueuedWorkRunRequest,
    ) -> Result<(), QueuedWorkRunError> {
        self.metrics.drive_passes.fetch_add(1, Ordering::Relaxed);
        self.inner.run_queued_work(request).await
    }

    async fn claim_and_run_pending_with_progress(
        &self,
        session_id: Option<&str>,
        reason: &str,
    ) -> Result<QueuedWorkRunProgress, QueuedWorkRunError> {
        self.metrics.drive_passes.fetch_add(1, Ordering::Relaxed);
        self.inner
            .claim_and_run_pending_with_progress(session_id, reason)
            .await
    }
}

fn instrumented_queued_work_port(
    engine_submitter: Arc<dyn QueuedWorkRunHandle>,
) -> (Arc<QueuedWorkMetrics>, Arc<dyn QueuedWorkSubstrate>) {
    let metrics = Arc::new(QueuedWorkMetrics::default());
    let instrumented_submitter = Arc::new(InstrumentedQueuedWorkRunHandle {
        inner: engine_submitter,
        metrics: Arc::clone(&metrics),
    });

    // Delegate the substrate contract to Lash's reference implementation. The
    // native driver keeps notifications contentless and coalesced, applies its
    // bounded retry policy, and performs one idempotent claim/submit pass for a
    // drain. The durable store or external engine remains the authority for
    // claim idempotency; this wrapper adds observation, never queue state.
    let port: Arc<dyn QueuedWorkSubstrate> =
        Arc::new(NativeQueuedWork::new(instrumented_submitter));
    (metrics, port)
}

fn core_with_caller_owned_queued_work(
    provider: ProviderHandle,
    model: lash::ModelSpec,
    store_factory: Arc<dyn lash::persistence::SessionStoreFactory>,
    queued_work: Arc<dyn QueuedWorkSubstrate>,
) -> lash::Result<lash::LashCore> {
    lash::LashCore::standard_builder(lash::TurnBudget::bounded(50))
        // Unlike `with_native_queued_work()`, this installs the exact port the
        // caller owns. Keep an Arc clone if the deployment also drives it from
        // a broker consumer, scheduler, or operator endpoint.
        .with_queued_work(queued_work)
        .provider(provider)
        .model(model)
        .store_factory(store_factory)
        .effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(crate::example_process_owner())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use lash::TurnInput;
    use lash::runtime::{QueuedWorkRunRequest, SessionWorkTarget};
    use tokio::sync::mpsc;

    use super::*;

    struct ChannelEngineSubmitter {
        requests: mpsc::UnboundedSender<QueuedWorkRunRequest>,
    }

    #[async_trait]
    impl QueuedWorkRunHandle for ChannelEngineSubmitter {
        async fn run_queued_work(
            &self,
            request: QueuedWorkRunRequest,
        ) -> Result<(), QueuedWorkRunError> {
            self.requests.send(request).map_err(|_| {
                QueuedWorkRunError::terminal(lash::plugins::PluginError::Session(
                    "fixture engine receiver closed".to_string(),
                ))
            })
        }
    }

    #[tokio::test]
    async fn caller_owned_queued_work_port_forwards_notify_and_drain() {
        let data_dir = tempfile::tempdir().expect("temporary docs-snippet directory");
        let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.path().join("sessions"),
        ));
        let (requests_tx, mut requests_rx) = mpsc::unbounded_channel();
        let (metrics, queued_work) =
            instrumented_queued_work_port(Arc::new(ChannelEngineSubmitter {
                requests: requests_tx,
            }));
        let core = core_with_caller_owned_queued_work(
            crate::test_support::provider(),
            crate::test_support::model(),
            store_factory,
            Arc::clone(&queued_work),
        )
        .expect("caller-owned queued-work port must resolve");

        let session = core
            .session("caller-owned-port")
            .open()
            .await
            .expect("session must open");
        session
            .enqueue(TurnInput::text("run this through my queue"))
            .id("docs-custom-port")
            .send()
            .await
            .expect("queued input must be durably accepted");

        let notified = tokio::time::timeout(Duration::from_secs(2), requests_rx.recv())
            .await
            .expect("native delegate must dispatch the notification")
            .expect("fixture engine must remain connected");
        assert_eq!(notified.session_id.as_deref(), Some("caller-owned-port"));
        assert_eq!(notified.reason, "queued_turn_input");

        queued_work
            .drain_session_work(
                SessionWorkTarget::Session("caller-owned-port".to_string()),
                "operator_drain",
            )
            .await
            .expect("explicit drain must preserve the delegate result");
        let drained = tokio::time::timeout(Duration::from_secs(2), requests_rx.recv())
            .await
            .expect("native delegate must dispatch the drain")
            .expect("fixture engine must remain connected");
        assert_eq!(drained.session_id.as_deref(), Some("caller-owned-port"));
        assert_eq!(drained.reason, "operator_drain");

        assert_eq!(metrics.durable_peeks.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.drive_passes.load(Ordering::Relaxed), 2);
    }
}
