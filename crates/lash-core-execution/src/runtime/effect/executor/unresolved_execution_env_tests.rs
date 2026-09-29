use super::*;

/// A load failure that carries a park keeps it (FIG-3575): the child
/// parks on its replay divergence instead of recording a refusal. An
/// artifact store answers only `ArtifactStoreError`, which carries no
/// park, so the law runs over the load error itself.
#[test]
fn a_carried_park_keeps_its_own_code_and_cause() {
    let parked = unresolved_execution_env(
        "tool child",
        &crate::testing::process_execution_env_fixture_ref(),
        crate::runtime::ProcessExecutionEnvLoadError::Store(
            crate::PluginError::RuntimeEffectController(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::LashlangCellReplayDivergence,
                "diverged",
            )),
        ),
    );
    assert_eq!(
        parked.code,
        crate::RuntimeErrorCode::LashlangCellReplayDivergence
    );
    assert_eq!(parked.turn_failure_cause(), crate::TurnFailureCause::Parked);
    assert!(
        !parked
            .journal_disposition(crate::RuntimeEffectKind::LoadExecutionEnv)
            .is_retryable_derivation(),
        "a park is the load's recorded outcome"
    );
}
