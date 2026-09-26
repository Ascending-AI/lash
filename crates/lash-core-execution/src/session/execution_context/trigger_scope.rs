//! Free helpers split out of `execution_context.rs`: the error every
//! process-scoped capability raises when no durable process execution is
//! wired, and the trigger-owner-scope ruling shared by the context's
//! trigger accessors.

use super::SessionId;

pub(super) fn missing_process_execution_error() -> crate::RuntimeEffectControllerError {
    crate::RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::ProcessRegistryUnavailable,
        "process execution is unavailable outside a durable process execution",
    )
}

pub(super) fn resolve_trigger_owner_scope(
    root_session_id: &SessionId,
    originator: Option<&crate::ProcessOriginator>,
) -> Result<crate::TriggerOwnerScope, crate::PluginError> {
    match originator {
        Some(crate::ProcessOriginator::Host {
            scope: Some(binding_id),
        }) => crate::TriggerOwnerScope::host(binding_id.clone()),
        Some(crate::ProcessOriginator::Host { scope: None }) => Err(crate::PluginError::Session(
            "bare host authority cannot own user trigger subscriptions; use an explicit host binding"
                .to_string(),
        )),
        Some(crate::ProcessOriginator::Session { session_id, .. }) => {
            Ok(crate::TriggerOwnerScope::session(session_id.clone()))
        }
        None => Ok(crate::TriggerOwnerScope::session(root_session_id)),
    }
}
