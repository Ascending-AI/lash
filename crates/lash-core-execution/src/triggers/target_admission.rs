use super::{PluginError, TriggerSubscriptionDraft, validate_trigger_target};

/// Runs the process-engine registry's admission on a trigger registration's
/// target before the subscription becomes durable (FIG-1522).
///
/// The delivery side deliberately stays outside the per-start gate: a delivery
/// replays the target and identity the subscription recorded, so the admission
/// decision has to be made once, here, when that record is created. Without it
/// a registration naming an engine kind this host never registered would
/// produce starts that were admitted nowhere.
///
/// An engine target resolves its definition reference through the owning
/// engine, so the durable row pins the engine's authoritative signature rather
/// than whatever the registrant claimed; a target that names no definition is
/// admitted on its engine kind alone. Immutable IDs resolve their descriptor
/// and validate any signature claim before recording the target.
pub async fn admit_trigger_registration_target(
    registry: &crate::ProcessEngineRegistry,
    draft: &mut TriggerSubscriptionDraft,
) -> Result<(), PluginError> {
    validate_trigger_target(&draft.target)?;
    if let crate::ProcessInput::Definition {
        definition_id,
        signature_claim,
        ..
    } = &draft.target
    {
        let ports = registry.artifact_ports().ok_or_else(|| {
            PluginError::Session("definition artifact ports are unavailable".into())
        })?;
        let resolved = ports
            .read_definition(registry, definition_id)
            .await?
            .ok_or_else(|| {
                PluginError::Runtime(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::DefinitionMissing,
                    format!("definition `{definition_id}` is missing"),
                ))
            })?;
        if let Some(signature) = signature_claim {
            registry
                .verify_definition_claim(
                    &resolved.draft,
                    &crate::ProcessDefinition::new(definition_id.clone(), signature.clone()),
                )
                .await
                .map_err(PluginError::from)?;
        }
        draft.target_identity = crate::ProcessIdentity::labelled(
            resolved.draft.engine_kind().clone(),
            draft.target_identity.label.clone(),
        );
        draft.target_identity.definition_id = Some(definition_id.clone());
        return Ok(());
    }
    let Some(reference) = draft.target_identity.definition.clone() else {
        // Refuses an unregistered kind with the registry's own typed error.
        registry.require(draft.target_identity.kind.as_str())?;
        return Ok(());
    };
    let resolution = registry
        .resolve(&reference)
        .await
        .map_err(crate::PluginError::from)?;
    draft.target_identity = crate::ProcessIdentity::for_definition(
        reference.with_resolved_signature(resolution.signature),
        draft.target_identity.label.clone(),
    );
    Ok(())
}
