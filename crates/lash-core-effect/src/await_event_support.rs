use crate::{ExecutionScope, RuntimeError};

/// Refuse a session-bearing scope on the scope-retirement lever.
pub fn await_event_scope_not_retirable(scope: &ExecutionScope) -> RuntimeError {
    RuntimeError::new(
        crate::RuntimeErrorCode::AwaitEventScopeNotRetirable,
        format!(
            "await-event scope retirement covers process and runtime-operation scopes only; scope `{}` belongs to a session and is revoked through session revocation",
            scope.id()
        ),
    )
}
