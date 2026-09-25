//! Durable turn cancellation: the cancellation binding, the closure
//! obligations a cancel leaves, and the request record with its outcome
//! (ADR 0039, ADR 0101 §10).

use super::{SessionExecutionLeaseAuthority, StoreError};
use crate::SessionId;

/// Durable turn-cancellation capability.
#[async_trait::async_trait]
pub trait TurnCancelStore: Send + Sync {
    /// Persist or validate the one cancellation authority selected for this
    /// session and, for a Process or runtime-operation controller, its physical
    /// journal scope. Session-bound turns keep their exact canonical address in
    /// each closure authorization, so distinct turns may share this authority.
    /// The check occurs under the current execution fence before any session
    /// work and never replaces the original selection.
    async fn validate_turn_cancellation_binding(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        binding_id: &str,
        admitted_scope: &crate::ExecutionScope,
    ) -> Result<(), StoreError>;

    /// Authorize exact closure of one cancellation gate pair for the admitted
    /// session and binding. The recorded lease identity authenticates the
    /// proposal, but its liveness and generation do not fence final settlement.
    /// A vacant slot accepts this value, an identical retry adopts it, and a
    /// different occupied value or retired physical scope returns a typed refusal.
    /// Final publication additionally requires the session-head CAS.
    async fn authorize_turn_cancel_closure(
        &self,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        authorization: &crate::TurnCancelClosureAuthorization,
    ) -> Result<crate::TurnCancelClosureAuthorizationOutcome, StoreError>;

    /// Load every unconsumed closure obligation for the bound session after
    /// validating the current execution fence, selected binding, and any
    /// original non-session physical scope.
    async fn pending_turn_cancel_closures(
        &self,
        session_id: &SessionId,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        binding_id: &str,
        admitted_scope: &crate::ExecutionScope,
    ) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError>;

    /// Read unconsumed closure pins for lifecycle coordination without
    /// acquiring execution authority. This grants no right to settle or
    /// consume them; deletion and scope-retirement owners use it only to refuse
    /// destructive cleanup until an activation holder has drained the pins.
    async fn pending_turn_cancel_closure_pins(
        &self,
    ) -> Result<Vec<crate::TurnCancelClosureAuthorization>, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "pending_turn_cancel_closure_pins",
        })
    }

    /// Persist cancellation intent for one turn.
    ///
    /// Repeating the address returns the original request unless the incoming
    /// one is a timing escalation of it
    /// ([`TurnCancelRequest::escalates`](crate::TurnCancelRequest::escalates)):
    /// same undelivered-input disposition, stronger mode. A repeat that
    /// disagrees about the disposition is a conflict the authoritative gate
    /// refuses, and leaves both the row and its intent revision untouched.
    /// This row is provisional evidence until
    /// [`Self::reconcile_turn_cancel_winner`] projects the keyed-gate winner.
    /// If the turn's final receipt is already durable, implementations perform
    /// no write and may return the incoming request with no outcome rather than
    /// decode retained historical outcome payloads.
    async fn record_turn_cancel_request(
        &self,
        _request: crate::TurnCancelRequest,
    ) -> Result<crate::TurnCancelRequestRecord, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "record_turn_cancel_request",
        })
    }

    /// Read the durable cancellation request and any accumulated repair
    /// outcome for one turn.
    async fn turn_cancel_request(
        &self,
        _address: &crate::TurnAddress,
    ) -> Result<Option<crate::TurnCancelRequestRecord>, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "turn_cancel_request",
        })
    }

    /// Read only durable cancellation intent, without reconstructing affected
    /// input payloads. Recovery uses this after vacuum may have reclaimed
    /// payload tombstones belonging to an earlier repair of the same turn id.
    async fn turn_cancel_request_intent(
        &self,
        _address: &crate::TurnAddress,
    ) -> Result<crate::TurnCancelIntentSnapshot, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "turn_cancel_request_intent",
        })
    }

    /// Project the authoritative keyed-gate winner into durable request
    /// evidence without changing arbitration authority.
    async fn reconcile_turn_cancel_winner(
        &self,
        _address: &crate::TurnAddress,
        _observed: &crate::TurnCancelIntentSnapshot,
        _evidence: &crate::TurnCancellationEvidence,
    ) -> Result<bool, StoreError> {
        Err(StoreError::UnsupportedStoreOperation {
            operation: "reconcile_turn_cancel_winner",
        })
    }
}
