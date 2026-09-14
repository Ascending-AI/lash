//! Scope binding identity for durable turn-cancellation authority.

const PHYSICAL_SCOPE_BINDING_SEPARATOR: &str = "#lash-physical-scope:";
/// Bind a durable cancellation authority to the non-session physical scope
/// that owns its journal. Turn/session scopes are already tied to their
/// address and keep the deployment identity unchanged.
pub fn turn_control_binding_id_for_scope(
    base: &str,
    scope: &crate::ExecutionScope,
) -> Result<String, RuntimeError> {
    match scope.journal_identity() {
        Ok(identity) if scope.session_id().is_none() => Ok(format!(
            "{base}{PHYSICAL_SCOPE_BINDING_SEPARATOR}{}",
            identity.key()
        )),
        Ok(_) => Ok(base.to_string()),
        Err(error) => Err(error.into()),
    }
}
pub(crate) fn binding_id_admits_scope(binding_id: &str, scope: &crate::ExecutionScope) -> bool {
    match scope.journal_identity() {
        Ok(identity) if scope.session_id().is_none() => binding_id.ends_with(&format!(
            "{PHYSICAL_SCOPE_BINDING_SEPARATOR}{}",
            identity.key()
        )),
        Ok(_) => !binding_id.contains(PHYSICAL_SCOPE_BINDING_SEPARATOR),
        Err(_) => false,
    }
}
/// Select the scope persisted with a turn-closure authorization.
///
/// Session-bound controllers may be driving a queue drain or another turn when
/// they discover an orphan. The durable input row's turn address is the
/// canonical admission identity in that case. Process and runtime-operation
/// controllers with a journal-bound cancellation authority instead carry the physical identity selected before
/// session work began, so recovery must preserve it exactly. Store-owned Native
/// promises use the turn address even when ordinary effects run in an operation scope.
pub(crate) fn admitted_turn_cancel_scope(
    address: &crate::TurnAddress,
    controller_scope: &crate::ExecutionScope,
    binding_id: &str,
) -> crate::ExecutionScope {
    if controller_scope.session_id().is_some()
        || !binding_id.contains(PHYSICAL_SCOPE_BINDING_SEPARATOR)
    {
        address.execution_scope()
    } else {
        controller_scope.clone()
    }
}
