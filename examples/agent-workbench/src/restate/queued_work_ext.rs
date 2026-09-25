use lash::SessionId;

#[async_trait::async_trait]
pub(super) trait QueuedWorkExt {
    async fn drain_session(
        &self,
        session_id: &SessionId,
        reason: &str,
    ) -> Result<(), lash::plugins::PluginError>;
}

#[async_trait::async_trait]
impl QueuedWorkExt for lash::runtime::NativeQueuedWork {
    async fn drain_session(
        &self,
        session_id: &SessionId,
        reason: &str,
    ) -> Result<(), lash::plugins::PluginError> {
        self.drive_now(session_id, reason).await
    }
}
