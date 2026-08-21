use crate::store::RuntimePersistence;
use crate::{PluginSession, ToolCallRecord, TurnOutcome};

use super::ExecutionStateUpdate;

pub(super) struct FinalCommitInput<'a> {
    pub(super) returned_state: &'a crate::SessionSnapshot,
    pub(super) tool_calls: &'a [ToolCallRecord],
    pub(super) plugins: Option<&'a PluginSession>,
    pub(super) execution_state_update: ExecutionStateUpdate,
    pub(super) agent_frame_switch_materializes: bool,
    pub(super) store: Option<&'a (dyn RuntimePersistence + 'a)>,
    pub(super) usage_deltas: &'a [crate::store::RuntimeUsageDelta],
    pub(super) outcome: &'a TurnOutcome,
    pub(super) originating_queue_claims: Vec<crate::QueuedWorkCompletion>,
    pub(super) originating_turn_input_claims: Vec<crate::TurnInputCompletion>,
    pub(super) completed_queue_claims: Vec<crate::QueuedWorkCompletion>,
    pub(super) completed_turn_input_claims: Vec<crate::TurnInputCompletion>,
    pub(super) queue_claim_generations: std::collections::HashMap<String, u64>,
    pub(super) turn_input_claim_generations: std::collections::HashMap<String, u64>,
    pub(super) current_session_lease_generation: Option<u64>,
    pub(super) enqueued_queue_batches: Vec<crate::QueuedWorkBatchDraft>,
    pub(super) interrupted_turn_input_turn_id: Option<String>,
    pub(super) recorded_attachment_intent_ids: std::collections::BTreeSet<crate::AttachmentId>,
    pub(super) session_execution_lease_completion: Option<crate::SessionExecutionLeaseAuthority>,
}
