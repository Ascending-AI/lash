use super::control::{ExecutionScope, ExternalCompletionError};
use crate::RuntimeError;

impl ExternalCompletionError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            raw: None,
        }
    }
}

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
