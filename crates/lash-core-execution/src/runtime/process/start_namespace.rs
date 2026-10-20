//! Freeze the linked namespace decision before storing an engine start.
use super::*;

pub(super) async fn link(
    stores: &ProcessStartStores<'_>,
    claim: &ReferrerClaim,
    registration: &mut ProcessRegistration,
    staged: &mut Option<StagedEnv>,
) -> Result<Option<ProcessEnv>, RuntimeEffectControllerError> {
    let Some(env) = load_staged_env(stores, staged.as_ref()).await? else {
        return Ok(None);
    };
    let ProcessInput::Engine { kind, payload, .. } = registration.input.as_ref() else {
        return Ok(Some(env));
    };
    let linked = stores
        .engines
        .link_package_namespaces(kind, payload, env.clone())
        .await?;
    if linked == env {
        return Ok(Some(env));
    }
    let store = stores.env_store.ok_or_else(|| {
        RuntimeEffectControllerError::foreign(
            "process_env_store_unavailable",
            TurnFailureCause::Outcome,
            "linking namespaces requires the process environment store",
        )
    })?;
    let env_ref = linked
        .stable_ref()
        .map_err(|error| lash_execution_contracts::PluginError::Session(error.to_string()))?;
    let held = super::super::hold_process_env(
        &ProcessEnvPorts::admitting(store.as_ref(), stores.engines.components()),
        claim,
        &ProcessEnvSelection::New(linked.clone()),
    )
    .await
    .map(|_| ())
    .map_err(|error| match error {
        HoldEnvError::Fault(error) => error,
        HoldEnvError::Refused(refusal) => {
            lash_execution_contracts::PluginError::ProcessEnvRefused { refusal }
        }
    });
    let acquired = held_or_ended(claim, held)?.is_none();
    *staged = Some(StagedEnv {
        env_ref: env_ref.clone(),
        staged: acquired,
    });
    if let ProcessInput::Engine { launch, .. } = Arc::make_mut(&mut registration.input) {
        launch.env = env_ref;
    }
    Ok(Some(linked))
}
