use std::sync::Arc;

use crate::{InMemoryLiveReplayStore, SessionObservationEvent};

impl InMemoryLiveReplayStore {
    /// Reopen this in-memory store over the same preserved replay history.
    ///
    /// The returned handle deliberately keeps both the replay incarnation and
    /// shared event buffers. Test and conformance code must construct a fresh
    /// store instead when it cannot prove that both survived restart.
    #[doc(hidden)]
    pub fn reopen_preserving_history(&self) -> Self {
        self.clone_preserving_history()
    }

    /// Install a gate between replay visibility and subscriber notification.
    #[doc(hidden)]
    pub fn with_before_notification_gate_for_testing(
        self,
        gate: impl Fn(&[Arc<SessionObservationEvent>]) + Send + Sync + 'static,
    ) -> Self {
        self.with_before_notification_gate(gate)
    }
}
