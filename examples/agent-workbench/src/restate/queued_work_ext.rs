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
        use lash::runtime::QueuedWorkSubstrate as _;

        self.drain_session_work(
            lash::runtime::SessionWorkTarget::Session(SessionId::from(session_id.to_string())),
            reason,
        )
        .await
        .map(|_| ())
    }
}
