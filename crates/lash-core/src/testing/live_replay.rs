use std::sync::Arc;

use crate::{InMemoryLiveReplayStore, SessionObservationEvent};

impl InMemoryLiveReplayStore {
    /// Install a gate between replay visibility and subscriber notification.
    #[doc(hidden)]
    pub fn with_before_notification_gate_for_testing(
        self,
        gate: impl Fn(&[Arc<SessionObservationEvent>]) + Send + Sync + 'static,
    ) -> Self {
        self.with_before_notification_gate(gate)
    }
}
