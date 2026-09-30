use super::*;

#[derive(Clone)]
pub(in crate::runtime::session_manager) struct ChannelEventSink {
    pub(in crate::runtime::session_manager) tx: mpsc::Sender<SessionStreamEvent>,
}

#[async_trait::async_trait]
impl EventSink for ChannelEventSink {
    async fn emit(&self, event: SessionStreamEvent) {
        if !self.tx.is_closed() {
            let _ = self.tx.send(event).await;
        }
    }
}
