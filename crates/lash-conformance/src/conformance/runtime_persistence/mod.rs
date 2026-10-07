//! [`RuntimeStore`] conformance, organized by capability segment:
//! [`SessionCommitStore`](crate::SessionCommitStore) (head CAS, checkpoint
//! hydration, metadata, attachment manifest, turn-commit stamps),
//! [`RunStore`](crate::store::RunStore) (admission binding),
//! [`TurnInputStore`](crate::TurnInputStore), and
//! [`StoreMaintenance`](crate::StoreMaintenance).

use super::*;
use crate::facade_support::{SessionGraphFacadeOps, ToolStateFacadeOps};
use lash_core::testing::conformance_support::ToolStateConformanceAccess;
use lash_core::testing::store_fixtures::admit_at_checkpoint_for_test;
pub(super) use lash_core::testing::store_fixtures::commit_runtime_state_for_test;
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

mod identity_claims;
mod ingress_integrity;
pub use identity_claims::*;
mod append_receipts;
mod attachments_and_queue;
mod checkpoint_admissions;
mod enqueue_sequence_identity;
mod reopen_and_commit;
mod run_specs;
mod runtime_basics;
mod suite_and_receipts;
mod turn_input_batches;
mod turn_inputs_and_reopen;

/// Public implementation paths used only by the exported registration macros.
pub mod runtime_persistence_macro_support {
    pub use super::append_receipts::*;
    pub use super::attachments_and_queue::*;
    pub use super::checkpoint_admissions::*;
    pub use super::enqueue_sequence_identity::*;
    pub use super::identity_claims::*;
    pub use super::ingress_integrity::*;
    pub use super::reopen_and_commit::*;
    pub use super::run_specs::*;
    pub use super::runtime_basics::*;
    pub use super::suite_and_receipts::*;
    pub use super::turn_input_batches::*;
    pub use super::turn_inputs_and_reopen::*;
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
