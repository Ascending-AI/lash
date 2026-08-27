use super::*;

/// Backend-specific terminal attachment retained for compatibility while
/// deployment ports own recoverable reattachment outcomes.
#[async_trait::async_trait]
pub trait ProcessAttach: Send + Sync {
    async fn await_terminal(&self, process_id: &str) -> Result<ProcessAwaitOutput, PluginError>;
}
