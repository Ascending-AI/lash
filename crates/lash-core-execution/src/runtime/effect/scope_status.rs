//! Scope-status vocabulary shared by every effect-engine substrate.
//!
//! Retirement and admission refusals are part of the [`EffectHost`]
//! contract, not of any one journal implementation, so they live here rather
//! than inside a driver body. Callers that gate scope deletion on quiescence
//! report [`scope_not_quiescent`]; every admission path reports
//! [`scope_retired`] for a scope whose retirement tombstone exists.
//!
//! [`EffectHost`]: super::executor::EffectHost

use crate::{RuntimeEffectControllerError, RuntimeError, RuntimeErrorCode};

/// The refusal a quiescence-gated retirement reports for a scope that still
/// has live work: nothing was deleted or fenced, and the caller retries once
/// the work settles.
pub fn scope_not_quiescent(scope_id: &str) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorCode::EffectScopeNotQuiescent,
        format!(
            "effect scope `{scope_id}` still has in-progress effects or an open group; retirement deferred until it is quiescent"
        ),
    )
}

/// The refusal every admission path reports for a scope whose retirement
/// tombstone exists: the journal under it was deleted as unreachable, so a
/// late redrive must fail closed rather than re-execute under an empty journal.
pub fn scope_retired(scope_id: &str) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::EffectScopeRetired,
        format!(
            "effect scope `{scope_id}` has been retired: its journal was deleted as unreachable and the scope cannot be re-admitted"
        ),
    )
}
