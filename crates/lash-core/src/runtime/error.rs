use crate::SessionError;
use crate::SessionId;





/// Wrap a store commit failure for the session-facing API.
///
/// The typed arm is not a convenience list: every variant here is one a host is
/// expected to *match on* rather than log. `HeadRevisionConflict` in particular
/// is the concurrent-append outcome the host is told to refresh and retry from,
/// so collapsing it into `Protocol(String)` would leave string matching as the
/// only way to tell a lost head race from an unrelated store failure.
pub(super) fn session_commit_error(
    context: &str,
    source: crate::store::StoreError,
) -> SessionError {
    match source {
        source @ (crate::store::StoreError::Contended
        | crate::store::StoreError::SessionDeleted { .. }
        | crate::store::StoreError::SessionStateVersionNewerThanRuntime { .. }
        | crate::store::StoreError::SessionStateVersionUnsupported { .. }
        | crate::store::StoreError::HeadRevisionConflict { .. }
        | crate::store::StoreError::AppendOperationIdentityConflict { .. }
        | crate::store::StoreError::AppendReceiptRequestedNodeCountCorrupt { .. }
        | crate::store::StoreError::CommitNodeBudgetExceeded { .. }
        | crate::store::StoreError::CommitByteBudgetExceeded { .. }
        | crate::store::StoreError::CheckpointComponentEncodingVersionMismatch {
            ..
        }
        | crate::store::StoreError::RecordEncodingFailed { .. }) => SessionError::Store {
            context: context.to_string(),
            source,
        },
        source => SessionError::Protocol(format!("{context}: {source}")),
    }
}

#[cfg(test)]
mod store_commit_error_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod host_commit_outcome_tests;




















