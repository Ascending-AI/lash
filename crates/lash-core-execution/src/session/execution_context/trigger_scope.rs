//! Free helpers split out of `execution_context.rs`: the error every
//! process-scoped capability raises when no durable process execution is
//! wired, and the trigger-owner-scope ruling shared by the context's
//! trigger accessors.

pub(super) fn missing_process_execution_error() -> crate::RuntimeEffectControllerError {
    crate::RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::ProcessRegistryUnavailable,
        "process execution is unavailable outside a durable process execution",
    )
}

/// The owner scope a trigger command issued by `owner` belongs under, given
/// the originator of the process execution it runs inside (when it runs
/// inside one). A process always runs under its originator; a process with
/// none has no session to own the subscription and is refused. The registration tool and the host-operation path
/// resolve through this one ruling so a subscription owns the same scope
/// whichever route declared it.
pub fn resolve_trigger_owner_scope(
    owner: &crate::RuntimeOwner,
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
        None => match owner {
            crate::RuntimeOwner::Session(session_id) => {
                Ok(crate::TriggerOwnerScope::session(session_id))
            }
            crate::RuntimeOwner::Process(process_id) => Err(
                crate::runtime::not_a_session_runtime("trigger_owner_scope", process_id),
            ),
        },
    }
}
