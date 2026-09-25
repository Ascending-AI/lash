/// Private proof that the durable final commit returned an accepted result.
/// Only the turn boundary can construct this value.
pub(in crate::runtime) struct AcceptedTurnCommit {
    confirmed_usage: Vec<crate::store::RuntimeUsageDeltaIdentity>,
}

pub(super) fn execution_state_capture_error(err: crate::SessionError) -> crate::StoreError {
    crate::StoreError::ExecutionStateCaptureFailed {
        message: err.to_string(),
    }
}

impl AcceptedTurnCommit {
    pub(in crate::runtime::turn_boundary) fn new(
        confirmed_usage: Vec<crate::store::RuntimeUsageDeltaIdentity>,
    ) -> Self {
        Self { confirmed_usage }
    }

    pub(in crate::runtime) fn into_confirmed_usage(
        self,
    ) -> Vec<crate::store::RuntimeUsageDeltaIdentity> {
        self.confirmed_usage
    }
}
