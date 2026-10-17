use super::*;

/// A load failure that carries a park keeps it (FIG-3575): the child
/// parks on its replay divergence instead of recording a refusal. An
/// artifact store answers only `ArtifactStoreError`, which carries no
/// park, so the law runs over the load error itself.
#[test]
fn a_carried_park_keeps_its_own_code_and_cause() {
    let parked = unresolved_execution_env(
        "process attempt",
        &lash_execution_contracts::testing::process_execution_env_fixture_ref(),
        lash_execution_contracts::process::ProcessEnvLoadError::Store(
            lash_execution_contracts::plugin_error::PluginError::RuntimeEffectController(
                RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::LashVmCellReplayDivergence,
                    "diverged",
                ),
            ),
        ),
    );
    assert_eq!(
        parked.code,
        crate::RuntimeErrorCode::LashVmCellReplayDivergence
    );
    assert_eq!(parked.turn_failure_cause(), crate::TurnFailureCause::Parked);
    assert!(
        !parked
            .journal_disposition(crate::RuntimeEffectKind::LoadExecutionEnv)
            .is_retryable_derivation(),
        "a park is the load's recorded outcome"
    );
}
