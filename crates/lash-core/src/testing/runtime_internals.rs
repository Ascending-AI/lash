//! Crate-internal runtime surface the relocated `runtime::tests` integration
//! binaries reach for.
//!
//! Those suites were unit tests inside the library until FIG-3041 promoted them
//! to integration binaries, so the crate-root short paths they used
//! (`crate::X`, which is `pub(crate) use facade_support::*` plus a handful of
//! module-private re-exports) are no longer in scope for them. This module is
//! the `testing`-feature seam that keeps exactly that surface reachable without
//! widening the crate's shipped API.

pub use crate::attachments::test_capability::attachment_test_capability;
pub use crate::attachments::{
    AttachmentProducer, AttachmentSourcePolicy, OpenAttachmentSourcePolicy,
};
pub use crate::plugin::{
    ErasedPluginOperationOutcome, OpenAgentFrameRequest, PluginOperationSpec, RuntimeServices,
    SessionObservedProcessOutcome, SessionObservedProcessReceipt, SessionObserverIntent,
};
pub use crate::runtime::NormalizedItem;
pub use crate::runtime::assembly::classify_output_state;
pub use crate::runtime::assembly::{LlmStreamAccumulator, TurnAssembler};
pub use crate::runtime::effect::{RuntimeEffectControllerHandle, TurnCancelWait};
pub use crate::runtime::io::normalize_input_items;
pub use crate::runtime::turn_input_ingress::ingress_message_id;
pub use crate::runtime::turn_loop::ResidentSessionState;
pub use crate::runtime::turn_queue::{
    QueuedWorkBatch, QueuedWorkBatchDraft, QueuedWorkClaimBoundary, QueuedWorkPayload,
    SessionCommandSettlement, process_wake_batch_draft,
};
pub use crate::runtime::usage::{merge_ledger_entry_saturating, normalize_prompt_usage};
pub use crate::session::Session;
/// `session_model::transport_stream_events` is crate-private; the relocated
/// runtime suites call it through this wrapper so the crate's non-testing
/// public surface stays exactly as it was.
pub fn transport_stream_events(
    provider: &crate::ProviderHandle,
    requested: Option<tokio::sync::mpsc::UnboundedSender<crate::llm::types::LlmStreamEvent>>,
) -> Option<crate::llm::types::LlmEventSender> {
    crate::session_model::transport_stream_events(provider, requested)
}
pub use crate::store::{
    PersistedSessionRead, SessionHeadMeta, SessionHeadPayload, load_persisted_session_state,
};
pub use crate::tool_dispatch::{
    CheckpointMessageBuffer, ToolCallLaunch, ToolTriggerOutcomeBuffer, execute_final_tool_intents,
    resolve_callable_manifest_by_id,
};
pub use crate::tool_dispatch::{
    coordinate_prepared_tool_call_launch_with_execution_context, execute_once,
};

/// `runtime::causal::{direct_effect_invocation, direct_request_discriminator}`
/// are crate-private and sit under a version-bump gate that reads the file's
/// guarded shape, so the relocated effect suite reaches them through these
/// wrappers instead of widening them in place.
pub mod causal {
    pub fn direct_effect_invocation(
        execution_scope: &crate::ExecutionScope,
        session_id: &crate::SessionId,
        usage_source: &str,
        replay_discriminator: String,
        turn_id: Option<&crate::TurnId>,
        caused_by: Option<crate::CausalRef>,
    ) -> crate::RuntimeEffectInvocation {
        crate::runtime::causal::direct_effect_invocation(
            execution_scope,
            session_id,
            usage_source,
            replay_discriminator,
            turn_id,
            caused_by,
        )
    }

    pub fn direct_request_discriminator(
        explicit_replay: Option<&crate::RuntimeReplay>,
        caused_by: Option<&crate::CausalRef>,
        ordinal: u64,
    ) -> String {
        crate::runtime::causal::direct_request_discriminator(explicit_replay, caused_by, ordinal)
    }
}
