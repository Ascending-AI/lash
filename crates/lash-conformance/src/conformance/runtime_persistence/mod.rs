//! [`RuntimePersistence`] conformance, organized by capability segment:
//! [`SessionCommitStore`](crate::SessionCommitStore) (head CAS, checkpoint
//! hydration, metadata, attachment manifest, turn-commit stamps),
//! [`SessionExecutionLeaseStore`](crate::SessionExecutionLeaseStore),
//! [`QueuedWorkStore`](crate::QueuedWorkStore) (claim fencing),
//! [`TurnInputStore`](crate::TurnInputStore), and
//! [`StoreMaintenance`](crate::StoreMaintenance).

use super::*;
use crate::facade_support::{SessionGraphFacadeOps, ToolStateFacadeOps};
use lash_core::testing::conformance_support::SessionGraphConformanceAccess;
use lash_core::testing::conformance_support::ToolStateConformanceAccess;
use lash_core::testing::store_fixtures::claim_session_execution_lease_for_test;
pub(super) use lash_core::testing::store_fixtures::commit_runtime_state_for_test;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;

const CONTROLLED_LEASE_TTL_MS: u64 = 50;
const REALTIME_SCAFFOLDING_LEASE_TTL_MS: u64 = 500;
// Real database operations can be descheduled between claiming a lease and
// observing it. This is a harness stall allowance, not the semantic expiry
// boundary: controlled-clock backends still prove the 50 ms contract exactly.
const REALTIME_LEASE_STALL_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(5);
const REALTIME_LEASE_EXPIRY_POLL: std::time::Duration = std::time::Duration::from_millis(10);
const REALTIME_DELAYED_QUEUE_ROW_GAP_MS: u64 = 500;
const REALTIME_DELAYED_QUEUE_ROW_CROSSING_MARGIN_MS: u64 = 50;

/// How runtime-persistence conformance drives session-lease expiry.
#[derive(Clone)]
pub enum RuntimePersistenceLeaseTiming {
    /// The backend owns its clock (for example PostgreSQL transaction time).
    Realtime,
    /// The backend reads an injected clock advanced by the supplied callback.
    Controlled(std::sync::Arc<dyn Fn(u64) + Send + Sync>),
}

impl RuntimePersistenceLeaseTiming {
    pub fn controlled(advance: impl Fn(u64) + Send + Sync + 'static) -> Self {
        Self::Controlled(std::sync::Arc::new(advance))
    }

    pub(super) fn scaffolding_lease_ttl_ms(&self) -> u64 {
        match self {
            Self::Realtime => REALTIME_SCAFFOLDING_LEASE_TTL_MS,
            Self::Controlled(_) => CONTROLLED_LEASE_TTL_MS,
        }
    }

    fn advance_to_just_before_semantic_expiry(&self) {
        if let Self::Controlled(advance) = self {
            advance(CONTROLLED_LEASE_TTL_MS - 1);
        }
    }

    fn advance_to_semantic_expiry(&self) {
        if let Self::Controlled(advance) = self {
            advance(1);
        }
    }

    /// Let one durable queued-drain wait slice pass in this backend's own time
    /// domain: real sleeping where the backend owns its clock, an injected-clock
    /// advance otherwise.
    pub(super) async fn pass_wait_slice(&self, slice_ms: u64) {
        match self {
            Self::Realtime => {
                tokio::time::sleep(std::time::Duration::from_millis(slice_ms)).await;
            }
            Self::Controlled(advance) => advance(slice_ms),
        }
    }

    async fn wait_until_expired(&self) {
        match self {
            Self::Realtime => {
                tokio::time::sleep(std::time::Duration::from_millis(
                    REALTIME_SCAFFOLDING_LEASE_TTL_MS,
                ))
                .await;
            }
            Self::Controlled(advance) => advance(CONTROLLED_LEASE_TTL_MS),
        }
    }

    fn delayed_queue_row_available_at_ms(&self) -> u64 {
        match self {
            Self::Realtime => {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock is after Unix epoch")
                    .as_millis() as u64
                    + REALTIME_DELAYED_QUEUE_ROW_GAP_MS
            }
            Self::Controlled(_) => 4_102_444_800_000,
        }
    }

    async fn cross_delayed_queue_row_boundary(&self) {
        match self {
            Self::Realtime => {
                tokio::time::sleep(std::time::Duration::from_millis(
                    REALTIME_DELAYED_QUEUE_ROW_GAP_MS
                        + REALTIME_DELAYED_QUEUE_ROW_CROSSING_MARGIN_MS,
                ))
                .await
            }
            Self::Controlled(advance) => advance(4_102_444_800_000),
        }
    }
}

mod append_receipts;
mod attachments_and_queue;
mod checkpoint_claims;
mod leases;
mod queue_redrive;
mod suite_and_receipts;
mod turn_inputs_and_reopen;

/// Public implementation paths used only by the exported registration macros.
#[doc(hidden)]
pub mod runtime_persistence_macro_support {
    pub use super::append_receipts::*;
    pub use super::attachments_and_queue::*;
    pub use super::checkpoint_claims::*;
    pub use super::leases::*;
    pub use super::queue_redrive::*;
    pub use super::suite_and_receipts::*;
    pub use super::turn_inputs_and_reopen::*;
    pub use crate::conformance::durable_queued_drain_wait::*;
    pub use crate::conformance::plugin_state::*;
}

use append_receipts::*;
pub use append_receipts::{
    append_receipt_corrupt_identity_encoding_version_is_refused,
    append_request_receipt_replays_after_ancestor_superseded,
    inactive_append_ancestor_precedes_stale_head, tombstoned_old_leaf_is_rejected,
};
pub use attachments_and_queue::{
    queued_work_claims_supersede_across_session_lease_generations,
    queued_work_exact_claim_preserves_physical_order_and_key_breaks,
};
use checkpoint_claims::*;
pub use checkpoint_claims::{
    checkpoint_claim_probe_transaction_counts, checkpoint_rejects_unknown_component_ref,
    complete_runtime_checkpoint_component_set_survives_cold_reopens, queued_process_wake_draft,
};
use leases::*;
pub use leases::{
    borrowed_session_execution_lease_commit_contract,
    same_host_distinct_executors_are_lane_less_without_revoking_holder,
    session_execution_lease_displacement, session_execution_lease_fence_authority,
};
pub use queue_redrive::{
    queued_work_redrive_selects_claim_identity_across_ready_gap,
    same_generation_claim_scans_reach_rows_beyond_the_scan_surplus,
};
pub use suite_and_receipts::{
    UnboundSessionAdmissionState, UnboundSessionResolutionHandles,
    runtime_persistence_clock_expiry, unbound_session_meta_refuses_ambiguous_resolution,
    unbound_session_reads_resolve_the_same_session,
};
pub use turn_inputs_and_reopen::{
    a_turn_that_cannot_commit_leaves_no_input_pinned_to_it,
    active_turn_input_claim_reacquires_after_unrecorded_checkpoint,
    turn_input_claims_supersede_across_session_lease_generations,
};
