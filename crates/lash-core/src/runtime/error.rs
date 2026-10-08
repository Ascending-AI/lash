use crate::SessionError;

pub use lash_core_store::runtime_error::{
    ExecutableGeneration, ExecutableGenerationRefusal, RecordedRefusal, RuntimeError,
    RuntimeErrorCause, RuntimeErrorCode, SessionStateVersionRefusal, StoredDataCorruption,
    TurnFailureCause, runtime_error_from_store_commit, runtime_error_from_turn_input_admission,
};

/// A store error met while committing, with its typed source kept: a host
/// matches on the store's own variant, and its class is the store's
/// ([`StoreError::runtime_code`](crate::store::StoreError::runtime_code)),
/// never read out of a message.
pub(super) fn session_commit_error(
    context: &str,
    source: crate::store::StoreError,
) -> SessionError {
    SessionError::Store {
        context: context.to_string(),
        source,
    }
}

#[cfg(test)]
mod store_commit_error_tests;

#[cfg(test)]
mod host_commit_outcome_tests;
