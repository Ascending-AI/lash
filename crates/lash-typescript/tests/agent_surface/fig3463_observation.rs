use super::*;

#[derive(Default)]
struct ObservedProcessAwaitFailureHost {
    observations: std::sync::Mutex<Vec<lashlang::LashlangExecutionObservation>>,
}

impl ExecutionHost for ObservedProcessAwaitFailureHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        ProcessAwaitFailureHost::Typed.perform(op).await
    }

    fn observe_lashlang_execution(&self, observation: lashlang::LashlangExecutionObservation) {
        self.observations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(observation);
    }
}

#[test]
fn direct_process_handle_await_node_failure_keeps_typed_provenance() {
    let host = ObservedProcessAwaitFailureHost::default();
    let _ = caught_process_await(&host, "error.message");
    let observations = host
        .observations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let failure = observations
        .iter()
        .find_map(|observation| match observation {
            lashlang::LashlangExecutionObservation::NodeFailed { failure, .. } => Some(failure),
            _ => None,
        });
    assert!(matches!(
        failure,
        Some(lashlang::LashlangExecutionFailure::Effect(effect))
            if effect.class == lash_sansio::ToolFailureClass::PermissionDenied
                && effect.code == "approval_denied"
                && effect.source == lash_sansio::ToolFailureSource::Policy
                && effect.retry == lash_sansio::ToolRetryStatus::Exhausted { attempts: 3 }
                && effect.replay_key == "await-effect-key"
    ));
}
