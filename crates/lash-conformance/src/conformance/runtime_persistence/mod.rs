//! [`RuntimePersistence`] conformance, organized by capability segment:
//! [`SessionCommitStore`](crate::SessionCommitStore) (head CAS, checkpoint
//! hydration, metadata, attachment manifest, turn-commit stamps),
//! [`RootStore`](crate::store::RootStore) (admission binding),
//! [`IngressStore`](crate::IngressStore), and
//! [`StoreMaintenance`](crate::StoreMaintenance).

use super::*;
use crate::facade_support::{SessionGraphFacadeOps, ToolStateFacadeOps};
use lash_core::testing::conformance_support::ToolStateConformanceAccess;
pub(super) use lash_core::testing::store_fixtures::commit_runtime_state_for_test;
use lash_core::testing::store_fixtures::seal_drive_fence_for_test;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::TurnId;

/// Clock policy supplied to runtime-persistence conformance.
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
}

mod admission_base_retention;
mod admission_laws;
mod append_receipts;
mod attachments_and_queue;
mod checkpoint_admissions;
mod enqueue_sequence_identity;
mod pending_follow_on;
mod queue_redrive;
mod reopen_and_commit;
mod root_admissions;
mod root_terminals;
mod run_specs;
mod runtime_basics;
mod suite_and_receipts;
mod turn_input_batches;
mod turn_inputs_and_reopen;
mod turn_parks;

/// Public implementation paths used only by the exported registration macros.
pub mod runtime_persistence_macro_support {
    pub use super::admission_base_retention::*;
    pub use super::admission_laws::*;
    pub use super::append_receipts::*;
    pub use super::attachments_and_queue::*;
    pub use super::checkpoint_admissions::*;
    pub use super::enqueue_sequence_identity::*;
    pub use super::pending_follow_on::*;
    pub use super::queue_redrive::*;
    pub use super::reopen_and_commit::*;
    pub use super::root_admissions::*;
    pub use super::root_terminals::*;
    pub use super::run_specs::*;
    pub use super::runtime_basics::*;
    pub use super::suite_and_receipts::*;
    pub use super::turn_input_batches::*;
    pub use super::turn_inputs_and_reopen::*;
    pub use super::turn_parks::*;
    pub use crate::conformance::plugin_state::*;
}

use append_receipts::*;
pub use append_receipts::{
    append_receipt_corrupt_identity_encoding_version_is_refused,
    append_request_receipt_replays_after_ancestor_superseded,
    inactive_append_ancestor_precedes_stale_head, tombstoned_old_leaf_is_rejected,
};
use checkpoint_admissions::*;
pub use checkpoint_admissions::{
    checkpoint_admission_probe_transaction_counts, checkpoint_rejects_unknown_component_ref,
    complete_runtime_checkpoint_component_set_survives_cold_reopens, queued_process_wake_draft,
};

pub use suite_and_receipts::{
    UnboundSessionAdmissionState, UnboundSessionResolutionHandles,
    unbound_session_meta_refuses_ambiguous_resolution,
    unbound_session_reads_resolve_the_same_session,
};
pub use turn_inputs_and_reopen::{
    a_checkpoint_admission_rerun_returns_its_own_rows,
    a_turn_that_cannot_commit_leaves_no_input_pinned_to_it,
};
