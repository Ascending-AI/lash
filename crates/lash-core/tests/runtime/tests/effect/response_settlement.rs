use super::*;
use lash_core::testing::TestTurnExecution as _;

const SEED: u64 = 0x5_e215;

struct SettlementExecutor {
    calls: std::sync::atomic::AtomicUsize,
    first_started: AtomicBool,
    started: tokio::sync::Notify,
    unsettled: AtomicBool,
    dispositions: Mutex<Vec<lash_core::plugin::CodeExecutionOutcome>>,
    nested_error: bool,
}

impl SettlementExecutor {
    fn new(nested_error: bool) -> Self {
        Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
            first_started: AtomicBool::new(false),
            started: tokio::sync::Notify::new(),
            unsettled: AtomicBool::new(false),
            dispositions: Mutex::new(Vec::new()),
            nested_error,
        }
    }

    async fn wait_for_first_execution(&self) {
        loop {
            let notified = self.started.notified();
            if self.first_started.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    fn dispositions(&self) -> Vec<lash_core::plugin::CodeExecutionOutcome> {
        self.dispositions.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl lash_core::plugin::CodeExecutorPlugin for SettlementExecutor {
    async fn frame_switch_carries(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        _successor: &lash_core::FrameNodeId,
        _initial_nodes: &[lash_core::SessionAppendNode],
    ) -> Result<Vec<lash_core::ArtifactName>, lash_core::SessionError> {
        Ok(Vec::new())
    }

    async fn execute_code(
        &self,
        ctx: lash_core::RuntimeExecutionContext<'_>,
        _request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, lash_core::SessionError> {
        if self.unsettled.swap(true, Ordering::SeqCst) {
            return Err(lash_core::SessionError::Protocol(
                "the previous code execution response has not been settled".to_string(),
            ));
        }
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_started.store(true, Ordering::SeqCst);
            self.started.notify_waiters();
            // A code executor reaches its turn's cancellation only through
            // recorded checkpoints (FIG-3672 P9), never a live probe.
            let mut checkpoint = 0;
            loop {
                checkpoint += 1;
                if ctx.turn_cancel_checkpoint(checkpoint).await.unwrap_or(true) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            if self.nested_error {
                ctx.record_nested_effect_error(lash_core::RuntimeEffectControllerError::foreign(
                    "injected_exec_handoff_failure",
                    lash_core::TurnFailureCause::LiveFault,
                    "injected code-effect response handoff failure",
                ));
                return Err(lash_core::SessionError::Protocol(
                    "code execution stopped at the response handoff".to_string(),
                ));
            }
            return Ok(lash_core::ExecResponse {
                output_archive: None,
                observations: Vec::new(),
                calls: Vec::new(),
                printed_images: Vec::new(),
                error: Some(lash_core::CellFailure::new(
                    lash_core::CellFailureKind::Host,
                    "code execution stopped",
                )),
                degraded_bindings: Vec::new(),
                terminal_finish: None,
                terminal_finish_retained: None,
                suspended: false,
            });
        }
        Ok(lash_core::ExecResponse {
            output_archive: None,
            observations: vec![lash_core::Observation {
                text: "next cell executed".to_string(),
                value: serde_json::json!("next cell executed"),
                projection: Default::default(),
            }],
            calls: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            degraded_bindings: Vec::new(),
            terminal_finish: None,
            terminal_finish_retained: None,
            suspended: false,
        })
    }

    async fn settle_code_execution(
        &self,
        disposition: lash_core::plugin::CodeExecutionOutcome,
    ) -> Result<(), lash_core::SessionError> {
        self.dispositions.lock_recover().push(disposition);
        self.unsettled.store(false, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Debug)]
struct ManualClock {
    epoch_ms: std::sync::atomic::AtomicU64,
}

impl ManualClock {
    fn new(epoch_ms: u64) -> Self {
        Self {
            epoch_ms: std::sync::atomic::AtomicU64::new(epoch_ms),
        }
    }

    fn advance_ms(&self, delta_ms: u64) {
        self.epoch_ms.fetch_add(delta_ms, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl lash_core::Clock for ManualClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let timestamp_ms = self.epoch_ms.load(Ordering::SeqCst);
        chrono::DateTime::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(timestamp_ms),
        )
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}
