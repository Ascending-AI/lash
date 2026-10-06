use super::*;
use lash_core::testing::TestTurnExecution as _;

const SEED: u64 = 0x5_a300;

#[derive(Clone)]
pub(super) struct CountingEchoTool {
    pub(super) executions: Arc<AtomicUsize>,
}

#[derive(Clone)]
pub(super) struct CancellationGatedTurnEvents {
    events: RecordingTurnEvents,
    cancellation: CancellationToken,
    entered: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl CancellationGatedTurnEvents {
    pub(super) fn new(
        cancellation: CancellationToken,
    ) -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        (
            Self {
                events: RecordingTurnEvents::default(),
                cancellation,
                entered: Arc::new(Mutex::new(Some(entered_tx))),
            },
            entered_rx,
        )
    }

    pub(super) fn snapshot(&self) -> Vec<TurnActivity> {
        self.events.snapshot()
    }
}

#[async_trait::async_trait]
impl lash_core::facade_support::TurnActivitySink for CancellationGatedTurnEvents {
    async fn emit(&self, activity: TurnActivity) {
        if matches!(
            &activity.event,
            TurnEvent::AssistantProseDelta { text, .. }
                if text.as_ref() == "drained before effect abort"
        ) {
            if let Some(entered) = self.entered.lock_recover().take() {
                let _ = entered.send(());
            }
            self.cancellation.cancelled().await;
        }
        self.events.events.lock_recover().push(activity);
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for CountingEchoTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        EchoTool.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        EchoTool.resolve_contract(name)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        EchoTool.execute(call).await
    }
}
