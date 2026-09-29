use crate::TurnId;
use crate::runtime::turn_settlement::TurnIngressSettlement;
use crate::{OmittedToolCalls, PluginSession, ToolCallRecord, TurnOutcome};

use super::ExecutionStateUpdate;

pub(super) struct FinalCommitInput<'a> {
    /// The turn's returned state, owned: the final state adopts its graph
    /// without a second holder, so the commit rewrites its drafts in place.
    pub(super) returned_state: crate::SessionSnapshot,
    pub(super) tool_calls: &'a [ToolCallRecord],
    pub(super) omitted: Option<&'a OmittedToolCalls>,
    pub(super) plugins: Option<&'a PluginSession>,
    pub(super) execution_state_update: ExecutionStateUpdate,
    pub(super) agent_frame_switch_materializes: bool,
    pub(super) store: Option<&'a crate::store::SessionStore>,
    pub(super) usage_deltas: &'a [crate::store::RuntimeUsageDelta],
    pub(super) failure_evidence: &'a [crate::TurnFailureEvidence],
    pub(super) outcome: &'a TurnOutcome,
    pub(super) ingress_settlement: TurnIngressSettlement,
    /// The follow-on the head owes once this commit publishes (ADR 0101 §3).
    pub(super) pending_follow_on: Option<crate::store::PendingFollowOn>,
    pub(super) interrupted_turn_input_turn_id: Option<TurnId>,
    pub(super) interrupted_turn_input_cancellation: Option<crate::TurnCancellationEvidence>,
    pub(super) interrupted_turn_cancel_intent: Option<crate::TurnCancelIntentSnapshot>,
    pub(super) turn_cancel_closure_settlement: Option<crate::TurnCancelClosureSettlement>,
    pub(super) turn_control_resolver: Option<&'a dyn crate::AwaitEventResolver>,
    pub(super) recorded_attachment_intent_ids: std::collections::BTreeSet<crate::AttachmentId>,
}
