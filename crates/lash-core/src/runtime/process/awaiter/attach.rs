use super::*;

/// Backend-specific terminal attachment. [`PluginError::ProcessAttachCeilingElapsed`]
/// means only the connection aged out; the durable wait remains live, so the
/// host must re-attach with the same process id instead of reporting failure.
#[async_trait::async_trait]
pub trait ProcessAttach: Send + Sync {
    async fn await_terminal(&self, process_id: &str) -> Result<ProcessAwaitOutput, PluginError>;
}
