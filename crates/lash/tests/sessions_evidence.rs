//! Compile-time witnesses for session- and turn-area facade and integrator contracts.
//!
//! FIG-2107 drains the ledger's remaining `unused-justify` slices: at the
//! dispatch-time recount this area held 525 rows. The 497 rows whose item
//! still exists are type-checked here through the path a host or integrator
//! would name — `lash::` for facade surface, `lash_core::` for internal seams
//! the integrator classes consume directly. The 28 rows whose item no longer
//! exists anywhere in this workspace are listed in the pull request rather
//! than witnessed here.

#![cfg(feature = "testing")]
#![allow(dead_code, unreachable_code, unused_variables, unused_imports)]
#![allow(clippy::all)]

fn type_witness<T>() {}
fn member_witness<T>(_: T) {}
fn field_witness<T>(_: impl FnOnce(&T)) {}
fn variant_witness<T>(_: impl FnOnce(&T) -> bool) {}

fn drain_area_witnesses() {
    // W0001: lash::AwaitEventWaitIdentity [enum]
    type_witness::<lash::AwaitEventWaitIdentity>();
    // W0002: lash::runtime::LashRuntime::set_turn_phase_probe [function]
    let _ = lash::runtime::LashRuntime::set_turn_phase_probe;
    // W0003: lash::runtime::LashRuntime::set_turn_phase_probe_if_changed [function]
    let _ = lash::runtime::LashRuntime::set_turn_phase_probe_if_changed;
    // W0008: lash::EmbedError::MissingTurnBudget [variant]
    variant_witness(|value: &lash::EmbedError| {
        matches!(value, lash::EmbedError::MissingTurnBudget)
    });
    // W0009: lash::TurnExecutionMetrics [struct]
    type_witness::<lash::TurnExecutionMetrics>();
    // W0010: lash::TurnExecutionMetrics::duration_ms [field]
    field_witness(|value: &lash::TurnExecutionMetrics| {
        let _ = &value.duration_ms;
    });
    // W0011: lash::TurnExecutionMetrics::had_code_execution [field]
    field_witness(|value: &lash::TurnExecutionMetrics| {
        let _ = &value.had_code_execution;
    });
    // W0012: lash::TurnExecutionMetrics::started_at_ms [field]
    field_witness(|value: &lash::TurnExecutionMetrics| {
        let _ = &value.started_at_ms;
    });
    // W0013: lash::ExternalCompletionError::code [field]
    field_witness(|value: &lash::ExternalCompletionError| {
        let _ = &value.code;
    });
    // W0014: lash::ExternalCompletionError::message [field]
    field_witness(|value: &lash::ExternalCompletionError| {
        let _ = &value.message;
    });
    // W0015: lash::InputItem [enum]
    type_witness::<lash::InputItem>();
    // W0016: lash::PendingTurnInput [struct]
    type_witness::<lash::PendingTurnInput>();
    // W0017: lash::PendingTurnInputCancelReceipt [struct]
    type_witness::<lash::PendingTurnInputCancelReceipt>();
    // W0018: lash::SessionCommand [enum]
    type_witness::<lash::SessionCommand>();
    // W0019: lash::SessionCommand::ApplyConfigPatch::patch [field]
    field_witness(|value: &lash::SessionCommand| {
        if let lash::SessionCommand::ApplyConfigPatch { patch, .. } = value {
            let _ = patch;
        }
    });
    // W0020: lash::SessionCommand::kind [function]
    let _ = lash::SessionCommand::kind;
    // W0021: lash::SessionCommand::source_key [function]
    let _: fn(&lash::SessionCommand, &'static str) -> String = lash::SessionCommand::source_key;
    // W0022: lash::SessionCreateRequest::child [function]
    let _ = lash::SessionCreateRequest::child("", todo!(), todo!(), todo!());
    // W0023: lash::SessionCreateRequest::root [function]
    let _ = lash::SessionCreateRequest::root;
    // W0024: lash::SessionCreateRequest::with_caused_by [function]
    let _ = lash::SessionCreateRequest::with_caused_by;
    // W0026: lash::SessionCreateRequest::with_initial_nodes [function]
    let _ = lash::SessionCreateRequest::with_initial_nodes;
    // W0027: lash::SessionCreateRequest::with_subagent_context [function]
    let _ = lash::SessionCreateRequest::with_subagent_context;
    // W0028: lash::SessionError::SessionCommandCancelled [variant]
    variant_witness(|value: &lash::SessionError| {
        matches!(value, lash::SessionError::SessionCommandCancelled(..))
    });
    // W0029: lash::SessionError::SessionCommandCancelled::0 [field]
    field_witness(|value: &lash::SessionError| {
        if let lash::SessionError::SessionCommandCancelled(f0) = value {
            let _ = f0;
        }
    });
    // W0030: lash::SessionError::SessionCommandPending [variant]
    variant_witness(|value: &lash::SessionError| {
        matches!(value, lash::SessionError::SessionCommandPending(..))
    });
    // W0031: lash::SessionError::SessionCommandPending::0 [field]
    field_witness(|value: &lash::SessionError| {
        if let lash::SessionError::SessionCommandPending(f0) = value {
            let _ = f0;
        }
    });
    // W0032: lash::TurnBudget::max_turns [function]
    let _ = lash::TurnBudget::max_turns;
    // W0033: lash::TurnCancellationEvidence [struct]
    type_witness::<lash::TurnCancellationEvidence>();
    // W0034: lash::TurnCancellationEvidence::origin [field]
    field_witness(|value: &lash::TurnCancellationEvidence| {
        let _ = &value.origin;
    });
    // W0035: lash::TurnCancellationEvidence::reason [field]
    field_witness(|value: &lash::TurnCancellationEvidence| {
        let _ = &value.reason;
    });
    // W0036: lash::TurnCancellationEvidence::request_id [field]
    field_witness(|value: &lash::TurnCancellationEvidence| {
        let _ = &value.request_id;
    });
    // W0037: lash::TurnCause [struct]
    type_witness::<lash::TurnCause>();
    // W0038: lash::TurnCause::to_event_message [function]
    let _ = lash::TurnCause::to_event_message;
    // W0039: lash::TurnInputApplication [struct]
    type_witness::<lash::TurnInputApplication>();
    // W0040: lash::durability::RuntimeHostConfig [struct]
    type_witness::<lash::durability::RuntimeHostConfig>();
    // W0041: lash::durability::RuntimeHostConfig::clock [field]
    field_witness(|value: &lash::durability::RuntimeHostConfig| {
        let _ = &value.clock;
    });
    // W0042: lash::durability::RuntimeHostConfig::control [field]
    field_witness(|value: &lash::durability::RuntimeHostConfig| {
        let _ = &value.control;
    });
    // W0043: lash::durability::RuntimeHostConfig::durability [field]
    field_witness(|value: &lash::durability::RuntimeHostConfig| {
        let _ = &value.durability;
    });
    // W0045: lash::durability::RuntimeHostConfig::new [function]
    let _ = lash::durability::RuntimeHostConfig::new;
    // W0046: lash::durability::RuntimeHostConfig::tracing [field]
    field_witness(|value: &lash::durability::RuntimeHostConfig| {
        let _ = &value.tracing;
    });
    // W0047: lash::durability::RuntimeHostConfig::with_clock [function]
    let _ = lash::durability::RuntimeHostConfig::with_clock;
    // W0049: lash::durability::TerminationPolicy [struct]
    type_witness::<lash::durability::TerminationPolicy>();
    // W0050: lash::durability::TerminationPolicy::treat_missing_done_as_failure [field]
    field_witness(|value: &lash::durability::TerminationPolicy| {
        let _ = &value.treat_missing_done_as_failure;
    });
    // W0051: lash::messages::Message::is_transient [function]
    let _ = lash::messages::Message::is_transient;
    // W0052: lash::persistence::ChronologicalProjection::from_turn_view [function]
    let _ = lash::persistence::ChronologicalProjection::from_turn_view;
    // W0053: lash::persistence::ChronologicalProjection::into_entries [function]
    let _ = lash::persistence::ChronologicalProjection::into_entries;
    // W0054: lash::persistence::SessionStoreFactory::has_claimable_queued_work [function]
    fn meth_0054<T: lash::persistence::SessionStoreFactory>(_: &T) {
        let _ = T::has_claimable_queued_work;
    }
    // W0055: lash::plugins::AppendSessionNodesRequest [struct]
    type_witness::<lash::plugins::AppendSessionNodesRequest>();
    // W0056: lash::plugins::SessionAppendNode::message [function]
    let _ = lash::plugins::SessionAppendNode::message;
    // W0057: lash::plugins::SessionGraphService [trait]
    fn trait_witness_0057<T: lash::plugins::SessionGraphService>() {}
    // W0058: lash::plugins::SessionStateChangedContext::sessions [field]
    field_witness(|value: &lash::plugins::SessionStateChangedContext| {
        let _ = &value.sessions;
    });
    // W0059: lash::plugins::SessionStateService [trait]
    fn trait_witness_0059<T: lash::plugins::SessionStateService>() {}
    // W0060: lash::plugins::TurnTransformContext::scoped_effect_controller [field]
    field_witness(|value: &lash::plugins::TurnTransformContext| {
        let _ = &value.scoped_effect_controller;
    });
    // W0061: lash::plugins::TurnTransformContext::session_lifecycle [field]
    field_witness(|value: &lash::plugins::TurnTransformContext| {
        let _ = &value.session_lifecycle;
    });
    // W0062: lash::plugins::TurnTransformContext::sessions [field]
    field_witness(|value: &lash::plugins::TurnTransformContext| {
        let _ = &value.sessions;
    });
    // W0063: lash::runtime::AssembledTurn [struct]
    type_witness::<lash::runtime::AssembledTurn>();
    // W0064: lash::runtime::AssembledTurn::assistant_output [field]
    field_witness(|value: &lash::runtime::AssembledTurn| {
        let _ = &value.assistant_output;
    });
    // W0065: lash::runtime::AssembledTurn::errors [field]
    field_witness(|value: &lash::runtime::AssembledTurn| {
        let _ = &value.errors;
    });
    // W0066: lash::runtime::AssembledTurn::execution [field]
    field_witness(|value: &lash::runtime::AssembledTurn| {
        let _ = &value.execution;
    });
    // W0067: lash::runtime::AssembledTurn::outcome [field]
    field_witness(|value: &lash::runtime::AssembledTurn| {
        let _ = &value.outcome;
    });
    // W0068: lash::runtime::AssembledTurn::state [field]
    field_witness(|value: &lash::runtime::AssembledTurn| {
        let _ = &value.state;
    });
    // W0069: lash::runtime::AwaitEventResolver [trait]
    fn trait_witness_0069<T: lash::runtime::AwaitEventResolver>() {}
    // W0072: lash::runtime::RuntimeEffectController [trait]
    fn trait_witness_0072<T: lash::runtime::RuntimeEffectController>() {}
    // W0073: lash::runtime::RuntimeEffectControllerError [struct]
    type_witness::<lash::runtime::RuntimeEffectControllerError>();
    // W0074: lash::runtime::RuntimeEffectEnvelope [struct]
    type_witness::<lash::runtime::RuntimeEffectEnvelope>();
    // W0075: lash::runtime::RuntimeEffectKind [enum]
    type_witness::<lash::runtime::RuntimeEffectKind>();
    // W0076: lash::runtime::RuntimeEffectLocalExecutor [struct]
    type_witness::<lash::runtime::RuntimeEffectLocalExecutor>();
    // W0077: lash::runtime::RuntimeEffectLocalExecutor::with_turn_cancel_scope [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::with_turn_cancel_scope;
    // W0078: lash::runtime::RuntimeEffectOutcome [enum]
    type_witness::<lash::runtime::RuntimeEffectOutcome>();
    // W0079: lash::runtime::RuntimeError::is_retryable [function]
    let _ = lash::runtime::RuntimeError::is_retryable;
    // W0080: lash::runtime::RuntimeError::is_terminal [function]
    let _ = lash::runtime::RuntimeError::is_terminal;
    // W0081: lash::runtime::RuntimeErrorCode::AttachmentSourcePolicyDenied [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::AttachmentSourcePolicyDenied
        )
    });
    // W0082: lash::runtime::RuntimeErrorCode::EffectPanicked [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::EffectPanicked)
    });
    // W0083: lash::runtime::RuntimeErrorCode::Plugin [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::Plugin)
    });
    // W0084: lash::runtime::RuntimeErrorCode::TurnInputSettlementSuperseded [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::TurnInputSettlementSuperseded
        )
    });
    // W0093: lash::runtime::RuntimeErrorCode::ProcessPanicked [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::ProcessPanicked)
    });
    // W0094: lash::runtime::RuntimeErrorCode::ProcessRegistryUnavailable [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::ProcessRegistryUnavailable
        )
    });
    // W0095: lash::runtime::RuntimeErrorCode::ProcessSignalWaitCancelled [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::ProcessSignalWaitCancelled
        )
    });
    // W0096: lash::runtime::RuntimeErrorCode::ProcessSignalWaitTimeout [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::ProcessSignalWaitTimeout
        )
    });
    // W0098: lash::runtime::RuntimeErrorCode::EngineEffectHostRequiresHandlerScope [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineEffectHostRequiresHandlerScope
        )
    });
    // W0099: lash::runtime::RuntimeErrorCode::EngineJournaledEffectPoisoned [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineJournaledEffectPoisoned
        )
    });
    // W0100: lash::runtime::RuntimeErrorCode::EngineProcessAwait [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::EngineProcessAwait)
    });
    // W0101: lash::runtime::RuntimeErrorCode::EngineProcessAwaitAfterTurnCancel [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineProcessAwaitAfterTurnCancel
        )
    });
    // W0102: lash::runtime::RuntimeErrorCode::EngineProcessTurnCancelContextMissing [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineProcessTurnCancelContextMissing
        )
    });
    // W0103: lash::runtime::RuntimeErrorCode::EngineTurnCancelScopeMismatch [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineTurnCancelScopeMismatch
        )
    });
    // W0104: lash::runtime::RuntimeErrorCode::EngineTurnCancelScopeMissing [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineTurnCancelScopeMissing
        )
    });
    // W0105: lash::runtime::RuntimeErrorCode::RuntimeEffectAttachmentStore [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectAttachmentStore
        )
    });
    // W0106: lash::runtime::RuntimeErrorCode::RuntimeEffectGroupChildCancelled [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectGroupChildCancelled
        )
    });
    // W0107: lash::runtime::RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalDecode [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalDecode
        )
    });
    // W0108: lash::runtime::RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalHashInvariant [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectEnvelopeCanonicalHashInvariant
        )
    });
    // W0109: lash::runtime::RuntimeErrorCode::RuntimeEffectEnvelopeHash [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectEnvelopeHash
        )
    });
    // W0111: lash::runtime::RuntimeErrorCode::RuntimeEffectInvocationSubject [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectInvocationSubject
        )
    });
    // W0112: lash::runtime::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch
        )
    });
    // W0113: lash::runtime::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable
        )
    });
    // W0114: lash::runtime::RuntimeErrorCode::RuntimeEffectAssistantResponseHook [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectAssistantResponseHook
        )
    });
    // W0115: lash::runtime::RuntimeErrorCode::RuntimeEffectLocalTaskClosed [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectLocalTaskClosed
        )
    });
    // W0116: lash::runtime::RuntimeErrorCode::RuntimeEffectProcessTaskJoin [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectProcessTaskJoin
        )
    });
    // W0117: lash::runtime::RuntimeErrorCode::RuntimeEffectReplayRequired [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectReplayRequired
        )
    });
    // W0118: lash::runtime::RuntimeErrorCode::RuntimeEffectSleepCancelled [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectSleepCancelled
        )
    });
    // W0119: lash::runtime::RuntimeErrorCode::RuntimeEffectTaskJoin [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectTaskJoin
        )
    });
    // W0120: lash::runtime::RuntimeErrorCode::RuntimeEffectToolAttemptCallId [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectToolAttemptCallId
        )
    });
    // W0121: lash::runtime::RuntimeErrorCode::RuntimeEffectToolAttemptIndex [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectToolAttemptIndex
        )
    });
    // W0126: lash::runtime::RuntimeErrorCode::RuntimeEffectWrongOutcome [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectWrongOutcome
        )
    });
    // W0127: lash::runtime::RuntimeErrorCode::SqliteEffectReplayCorruptRow [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SqliteEffectReplayCorruptRow
        )
    });
    // W0128: lash::runtime::RuntimeErrorCode::SqliteEffectReplayDecode [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SqliteEffectReplayDecode
        )
    });
    // W0129: lash::runtime::RuntimeErrorCode::SqliteEffectReplayEncode [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SqliteEffectReplayEncode
        )
    });
    // W0130: lash::runtime::RuntimeErrorCode::SqliteEffectReplayHashConflict [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SqliteEffectReplayHashConflict
        )
    });
    // W0131: lash::runtime::RuntimeErrorCode::SqliteEffectReplayKeyMissing [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SqliteEffectReplayKeyMissing
        )
    });
    // W0132: lash::runtime::RuntimeErrorCode::SqliteEffectReplayLeaseLost [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SqliteEffectReplayLeaseLost
        )
    });
    // W0133: lash::runtime::RuntimeErrorCode::SqliteEffectReplayMissing [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SqliteEffectReplayMissing
        )
    });
    // W0134: lash::runtime::RuntimeErrorCode::SqliteEffectReplayStore [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SqliteEffectReplayStore
        )
    });
    // W0137: lash::runtime::RuntimeErrorCode::ToolCatalogResolutionFailed [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::ToolCatalogResolutionFailed
        )
    });
    // W0138: lash::runtime::RuntimeErrorCode::TriggerStoreUnavailable [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::TriggerStoreUnavailable
        )
    });
    // W0139: lash::runtime::RuntimeErrorCode::AwaitEventCancelUnsupported [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::AwaitEventCancelUnsupported
        )
    });
    // W0140: lash::runtime::RuntimeErrorCode::AwaitEventKeySign [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::AwaitEventKeySign)
    });
    // W0141: lash::runtime::RuntimeErrorCode::AwaitEventUnknownOrRevoked [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::AwaitEventUnknownOrRevoked
        )
    });
    // W0142: lash::runtime::RuntimeErrorCode::AwaitEventUnsupported [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::AwaitEventUnsupported
        )
    });
    // W0143: lash::runtime::RuntimeErrorCode::CancelStartGateUnavailable [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::CancelStartGateUnavailable
        )
    });
    // W0144: lash::runtime::RuntimeErrorCode::EffectJournalRetirementUnsupported [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EffectJournalRetirementUnsupported
        )
    });
    // W0147: lash::runtime::RuntimeErrorCode::InvalidAwaitEventSessionId [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::InvalidAwaitEventSessionId
        )
    });
    // W0148: lash::runtime::RuntimeErrorCode::InvalidAwaitEventWaitIdentity [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::InvalidAwaitEventWaitIdentity
        )
    });
    // W0149: lash::runtime::RuntimeErrorCode::InvalidTurnCancelRequest [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::InvalidTurnCancelRequest
        )
    });
    // W0150: lash::runtime::RuntimeErrorCode::LiveReplay [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::LiveReplay)
    });
    // W0151: lash::runtime::RuntimeErrorCode::LlmProvider [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::LlmProvider)
    });
    // W0158: lash::runtime::RuntimeErrorCode::QueuedWork [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::QueuedWork)
    });
    // W0159: lash::runtime::RuntimeErrorCode::ResidentSessionReloadFailed [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::ResidentSessionReloadFailed
        )
    });
    // W0160: lash::runtime::RuntimeErrorCode::EngineAwaitEventAwait [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineAwaitEventAwait
        )
    });
    // W0161: lash::runtime::RuntimeErrorCode::EngineAwaitEventCancel [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineAwaitEventCancel
        )
    });
    // W0163: lash::runtime::RuntimeErrorCode::EngineAwaitEventPeek [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::EngineAwaitEventPeek)
    });
    // W0164: lash::runtime::RuntimeErrorCode::EngineAwaitEventRevocationRead [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineAwaitEventRevocationRead
        )
    });
    // W0165: lash::runtime::RuntimeErrorCode::EngineAwaitEventRevoke [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineAwaitEventRevoke
        )
    });
    // W0166: lash::runtime::RuntimeErrorCode::EngineAwaitEventSessionUpdate [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineAwaitEventSessionUpdate
        )
    });
    // W0167: lash::runtime::RuntimeErrorCode::EngineEffectController [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineEffectController
        )
    });
    // W0168: lash::runtime::RuntimeErrorCode::EngineProcessTerminalEncode [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineProcessTerminalEncode
        )
    });
    // W0169: lash::runtime::RuntimeErrorCode::EngineTurnTerminalAttach [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineTurnTerminalAttach
        )
    });
    // W0170: lash::runtime::RuntimeErrorCode::EngineTurnTerminalAttachCeilingElapsed [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineTurnTerminalAttachCeilingElapsed
        )
    });
    // W0171: lash::runtime::RuntimeErrorCode::EngineTurnTerminalInvalidResolution [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::EngineTurnTerminalInvalidResolution
        )
    });
    // W0172: lash::runtime::RuntimeErrorCode::RuntimeEffectControllerTaskClosed [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectControllerTaskClosed
        )
    });
    // W0174: lash::runtime::RuntimeErrorCode::SessionCommandClaim [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::SessionCommandClaim)
    });
    // W0175: lash::runtime::RuntimeErrorCode::SessionCommandIdempotencyKey [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SessionCommandIdempotencyKey
        )
    });
    // W0176: lash::runtime::RuntimeErrorCode::SessionCommandPostDriveRefresh [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SessionCommandPostDriveRefresh
        )
    });
    // W0177: lash::runtime::RuntimeErrorCode::SessionCommandRefresh [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SessionCommandRefresh
        )
    });
    // W0178: lash::runtime::RuntimeErrorCode::SessionCommandRefreshTools [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SessionCommandRefreshTools
        )
    });
    // W0179: lash::runtime::RuntimeErrorCode::SessionDeleteScopeMismatch [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SessionDeleteScopeMismatch
        )
    });
    // W0180: lash::runtime::RuntimeErrorCode::SessionHeadRefresh [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::SessionHeadRefresh)
    });
    // W0181: lash::runtime::RuntimeErrorCode::SessionToolRegistry [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::SessionToolRegistry)
    });
    // W0182: lash::runtime::RuntimeErrorCode::SqliteAwaitEventDecode [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SqliteAwaitEventDecode
        )
    });
    // W0183: lash::runtime::RuntimeErrorCode::SqliteAwaitEventEncode [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SqliteAwaitEventEncode
        )
    });
    // W0184: lash::runtime::RuntimeErrorCode::SqliteAwaitEventNotify [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SqliteAwaitEventNotify
        )
    });
    // W0185: lash::runtime::RuntimeErrorCode::SqliteAwaitEventSign [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::SqliteAwaitEventSign)
    });
    // W0186: lash::runtime::RuntimeErrorCode::SqliteAwaitEventStore [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SqliteAwaitEventStore
        )
    });
    // W0187: lash::runtime::RuntimeErrorCode::SqliteEffectJournalRetirement [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::SqliteEffectJournalRetirement
        )
    });
    // W0188: lash::runtime::RuntimeErrorCode::QueuedWorkRowExceedsContextWindow [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::QueuedWorkRowExceedsContextWindow
        )
    });
    // W0189: lash::runtime::RuntimeErrorCode::ToolCompletionKeyMissingCallId [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::ToolCompletionKeyMissingCallId
        )
    });
    // W0190: lash::runtime::RuntimeErrorCode::ToolDeferralNotDeclared [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::ToolDeferralNotDeclared
        )
    });
    // W0192: lash::runtime::RuntimeErrorCode::TransientCancelWatch [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::TransientCancelWatch)
    });
    // W0193: lash::runtime::RuntimeErrorCode::TransientTerminalPublication [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::TransientTerminalPublication
        )
    });
    // W0194: lash::runtime::RuntimeErrorCode::TurnCancelGateDecode [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::TurnCancelGateDecode)
    });
    // W0195: lash::runtime::RuntimeErrorCode::TurnCancelGateEncode [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::TurnCancelGateEncode)
    });
    // W0196: lash::runtime::RuntimeErrorCode::TurnCancelGateInvalidTerminal [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::TurnCancelGateInvalidTerminal
        )
    });
    // W0197: lash::runtime::RuntimeErrorCode::TurnControlPeekOutcome [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::TurnControlPeekOutcome
        )
    });
    // W0198: lash::runtime::RuntimeErrorCode::TurnControlUnknownOrRevoked [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::TurnControlUnknownOrRevoked
        )
    });
    // W0199: lash::runtime::RuntimeErrorCode::TurnControlWaitCancelled [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::TurnControlWaitCancelled
        )
    });
    // W0200: lash::runtime::RuntimeErrorCode::TurnControlWaitTimeout [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::TurnControlWaitTimeout
        )
    });
    // W0201: lash::runtime::RuntimeErrorCode::TurnTerminalDecode [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::TurnTerminalDecode)
    });
    // W0202: lash::runtime::RuntimeErrorCode::TurnTerminalEncode [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(value, lash::runtime::RuntimeErrorCode::TurnTerminalEncode)
    });
    // W0203: lash::runtime::RuntimeErrorCode::TurnTerminalInvalidResolution [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::TurnTerminalInvalidResolution
        )
    });
    // W0204: lash::runtime::RuntimeErrorCode::TurnTerminalUnknownOrRevoked [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::TurnTerminalUnknownOrRevoked
        )
    });
    // W0205: lash::runtime::RuntimeErrorCode::from_wire_code [function]
    let _ = lash::runtime::RuntimeErrorCode::from_wire_code;
    // W0206: lash::runtime::RuntimeErrorCode::is_retryable [function]
    let _ = lash::runtime::RuntimeErrorCode::is_retryable;
    // W0207: lash::runtime::RuntimeErrorCode::is_terminal [function]
    let _ = lash::runtime::RuntimeErrorCode::is_terminal;
    // W0208: lash::runtime::RuntimeInvocation [struct]
    type_witness::<lash::runtime::RuntimeInvocation>();
    // W0210: lash::runtime::SessionPolicy [struct]
    type_witness::<lash::runtime::SessionPolicy>();
    // W0211: lash::runtime::SessionPolicy::context_window_tokens [function]
    let _ = lash::runtime::SessionPolicy::context_window_tokens;
    // W0212: lash::runtime::SessionSnapshot [struct]
    type_witness::<lash::runtime::SessionSnapshot>();
    // W0213: lash::runtime::SessionSnapshot::append_active_read_delta [function]
    let _ = lash::runtime::SessionSnapshot::append_active_read_delta;
    // W0214: lash::runtime::SessionSnapshot::new [function]
    let _ = lash::runtime::SessionSnapshot::new;
    // W0215: lash::runtime::SessionSnapshot::read_view [function]
    let _ = lash::runtime::SessionSnapshot::read_view;
    // W0216: lash::runtime::SessionSnapshot::replace_active_read_state [function]
    let _ = lash::runtime::SessionSnapshot::replace_active_read_state;
    // W0217: lash::runtime::TurnContext [struct]
    type_witness::<lash::runtime::TurnContext>();
    // W0223: lash::turn::AssistantOutput [struct]
    type_witness::<lash::turn::AssistantOutput>();
    // W0224: lash::turn::AssistantOutput::raw_text [field]
    field_witness(|value: &lash::turn::AssistantOutput| {
        let _ = &value.raw_text;
    });
    // W0225: lash::turn::AssistantOutput::safe_text [field]
    field_witness(|value: &lash::turn::AssistantOutput| {
        let _ = &value.safe_text;
    });
    // W0226: lash::turn::AssistantOutput::state [field]
    field_witness(|value: &lash::turn::AssistantOutput| {
        let _ = &value.state;
    });
    // W0227: lash::turn::TurnIssue [struct]
    type_witness::<lash::turn::TurnIssue>();
    // W0228: lash::turn::TurnIssue::code [field]
    field_witness(|value: &lash::turn::TurnIssue| {
        let _ = &value.code;
    });
    // W0229: lash::turn::TurnIssue::kind [field]
    field_witness(|value: &lash::turn::TurnIssue| {
        let _ = &value.kind;
    });
    // W0230: lash::turn::TurnIssue::message [field]
    field_witness(|value: &lash::turn::TurnIssue| {
        let _ = &value.message;
    });
    // W0231: lash::turn::TurnIssue::raw [field]
    field_witness(|value: &lash::turn::TurnIssue| {
        let _ = &value.raw;
    });
    // W0232: lash::turn::TurnIssue::retryable [field]
    field_witness(|value: &lash::turn::TurnIssue| {
        let _ = &value.retryable;
    });
    // W0233: lash::turn::TurnIssue::terminal_reason [field]
    field_witness(|value: &lash::turn::TurnIssue| {
        let _ = &value.terminal_reason;
    });
    // W0234: lash::plugins::AgentFrameAssignment [struct]
    type_witness::<lash::plugins::AgentFrameAssignment>();
    // W0235: lash::plugins::AgentFrameAssignment::from_policy [function]
    let _ = lash::plugins::AgentFrameAssignment::from_policy;
    // W0236: lash::plugins::AgentFrameAssignment::policy [field]
    field_witness(|value: &lash::plugins::AgentFrameAssignment| {
        let _ = &value.policy;
    });
    // W0238: lash::plugins::FrameNodeId [struct]
    type_witness::<lash::plugins::FrameNodeId>();
    // W0239: lash::plugins::FrameNodeId::as_str [function]
    let _ = lash::plugins::FrameNodeId::as_str;
    // W0240: lash::plugins::FrameNodeId::into_inner [function]
    let _ = lash::plugins::FrameNodeId::into_inner;
    // W0241: lash::plugins::AgentFrameReason [struct]
    type_witness::<lash::plugins::AgentFrameReason>();
    // W0242: lash::plugins::AgentFrameReason::as_str [function]
    let _ = lash::plugins::AgentFrameReason::as_str;
    // W0243: lash::plugins::AgentFrameReason::initial [function]
    let _ = lash::plugins::AgentFrameReason::initial;
    // W0244: lash::plugins::AgentFrameRecord [struct]
    type_witness::<lash::plugins::AgentFrameRecord>();
    // W0245: lash::plugins::AgentFrameRecord::assignment [field]
    field_witness(|value: &lash::plugins::AgentFrameRecord| {
        let _ = &value.assignment;
    });
    // W0246: lash::plugins::AgentFrameRecord::created_at [field]
    field_witness(|value: &lash::plugins::AgentFrameRecord| {
        let _ = &value.created_at;
    });
    // W0247: lash::plugins::AgentFrameRecord::frame_node_id [field]
    field_witness(|value: &lash::plugins::AgentFrameRecord| {
        let _ = &value.frame_node_id;
    });
    // W0248: lash::plugins::AgentFrameRecord::previous_frame_node_id [field]
    field_witness(|value: &lash::plugins::AgentFrameRecord| {
        let _ = &value.previous_frame_node_id;
    });
    // W0249: lash::plugins::AgentFrameRecord::reason [field]
    field_witness(|value: &lash::plugins::AgentFrameRecord| {
        let _ = &value.reason;
    });
    // W0250: lash::plugins::AgentFrameRecord::session_id [field]
    field_witness(|value: &lash::plugins::AgentFrameRecord| {
        let _ = &value.session_id;
    });
    // W0251: lash::durability::BoundaryReason [enum]
    type_witness::<lash::durability::BoundaryReason>();
    // W0253: lash::durability::BoundaryReason::JournalBudget [variant]
    variant_witness(|value: &lash::durability::BoundaryReason| {
        matches!(value, lash::durability::BoundaryReason::JournalBudget)
    });
    // W0254: lash::durability::EffectJournalRetirement [enum]
    type_witness::<lash::durability::EffectJournalRetirement>();
    // W0255: lash::durability::EffectJournalRetirement::Session [variant]
    variant_witness(|value: &lash::durability::EffectJournalRetirement| {
        matches!(
            value,
            lash::durability::EffectJournalRetirement::Session { .. }
        )
    });
    // W0256: lash::durability::EffectJournalRetirement::Session::session_id [field]
    field_witness(|value: &lash::durability::EffectJournalRetirement| {
        if let lash::durability::EffectJournalRetirement::Session { session_id, .. } = value {
            let _ = session_id;
        }
    });
    // W0257: lash::durability::EffectJournalRetirement::session [function]
    let _ = lash::durability::EffectJournalRetirement::session("");
    // W0258: lash::plugins::ExecRequest [struct]
    type_witness::<lash::plugins::ExecRequest>();
    // W0259: lash::plugins::ExecRequest::code [field]
    field_witness(|value: &lash::plugins::ExecRequest| {
        let _ = &value.code;
    });
    // W0260: lash::plugins::ExecRequest::language [field]
    field_witness(|value: &lash::plugins::ExecRequest| {
        let _ = &value.language;
    });
    // W0261: lash::persistence::LiveReplayOutcome [enum]
    type_witness::<lash::persistence::LiveReplayOutcome>();
    // W0262: lash::persistence::LiveReplayOutcome::Gap [variant]
    variant_witness(|value: &lash::persistence::LiveReplayOutcome| {
        matches!(value, lash::persistence::LiveReplayOutcome::Gap(..))
    });
    // W0263: lash::persistence::LiveReplayOutcome::Gap::0 [field]
    field_witness(|value: &lash::persistence::LiveReplayOutcome| {
        if let lash::persistence::LiveReplayOutcome::Gap(f0) = value {
            let _ = f0;
        }
    });
    // W0264: lash::persistence::LiveReplayOutcome::Replayed [variant]
    variant_witness(|value: &lash::persistence::LiveReplayOutcome| {
        matches!(value, lash::persistence::LiveReplayOutcome::Replayed(..))
    });
    // W0265: lash::persistence::LiveReplayOutcome::Replayed::0 [field]
    field_witness(|value: &lash::persistence::LiveReplayOutcome| {
        if let lash::persistence::LiveReplayOutcome::Replayed(f0) = value {
            let _ = f0;
        }
    });
    // W0266: lash::messages::Part::code [function]
    let _ = lash::messages::Part::code;
    // W0267: lash::messages::Part::error [function]
    let _ = lash::messages::Part::error;
    // W0268: lash::messages::Part::output [function]
    let _ = lash::messages::Part::output;
    // W0269: lash::messages::Part::prose [function]
    let _ = lash::messages::Part::prose;
    // W0270: lash::messages::Part::reasoning [function]
    let _ = lash::messages::Part::reasoning;
    // W0271: lash::messages::Part::text [function]
    let _ = lash::messages::Part::text;
    // W0272: lash_core::PreparedTurnMachine [type_alias]
    type_witness::<lash_core::PreparedTurnMachine>();
    // W0273: lash::plugins::RuntimeExecutionContext [struct]
    type_witness::<lash::plugins::RuntimeExecutionContext>();
    // W0274: lash::plugins::RuntimeExecutionContext::chronological_projection [function]
    let _ = lash::plugins::RuntimeExecutionContext::chronological_projection;
    // W0275: lash::plugins::RuntimeExecutionContext::execution_scope_id [function]
    let _ = lash::plugins::RuntimeExecutionContext::execution_scope_id;
    // W0276: lash::plugins::RuntimeExecutionContext::parent_invocation [function]
    let _ = lash::plugins::RuntimeExecutionContext::parent_invocation;
    // W0277: lash::plugins::RuntimeExecutionContext::session_id [function]
    let _ = lash::plugins::RuntimeExecutionContext::session_id;
    // W0278: lash::plugins::RuntimeExecutionContext::session_scope [function]
    let _ = lash::plugins::RuntimeExecutionContext::session_scope;
    // W0279: lash::plugins::RuntimeExecutionContext::turn_context [function]
    let _ = lash::plugins::RuntimeExecutionContext::turn_context;
    // W0280: lash_core::SansIoTurnInput [type_alias]
    type_witness::<lash_core::SansIoTurnInput>();
    // W0281: lash::plugins::SegmentHandover [struct]
    type_witness::<lash::plugins::SegmentHandover>();
    // W0282: lash::plugins::SegmentHandover::engine_state [field]
    field_witness(|value: &lash::plugins::SegmentHandover| {
        let _ = &value.engine_state;
    });
    // W0283: lash::plugins::SegmentHandover::program_hash [field]
    field_witness(|value: &lash::plugins::SegmentHandover| {
        let _ = &value.program_hash;
    });
    // W0284: lash::plugins::SegmentHandover::reason [field]
    field_witness(|value: &lash::plugins::SegmentHandover| {
        let _ = &value.reason;
    });
    // W0285: lash::durability::SegmentProgress [struct]
    type_witness::<lash::durability::SegmentProgress>();
    // W0286: lash::durability::SegmentProgress::effects_executed [field]
    field_witness(|value: &lash::durability::SegmentProgress| {
        let _ = &value.effects_executed;
    });
    // W0287: lash::durability::SegmentProgress::journaled_bytes_estimate [field]
    field_witness(|value: &lash::durability::SegmentProgress| {
        let _ = &value.journaled_bytes_estimate;
    });
    // W0289: lash::process::SessionId [type_alias] (rehomed)
    type_witness::<lash::SessionId>();
    // W0290: lash::plugins::TurnDriverPreamble [type_alias]
    type_witness::<lash::plugins::TurnDriverPreamble>();
    // W0291: lash::persistence::TurnInputClaimMode [enum]
    type_witness::<lash::persistence::TurnInputClaimMode>();
    // W0292: lash::persistence::TurnInputClaimMode::ActiveTurn [variant]
    variant_witness(|value: &lash::persistence::TurnInputClaimMode| {
        matches!(
            value,
            lash::persistence::TurnInputClaimMode::ActiveTurn { .. }
        )
    });
    // W0293: lash::persistence::TurnInputClaimMode::ActiveTurn::turn_id [field]
    field_witness(|value: &lash::persistence::TurnInputClaimMode| {
        if let lash::persistence::TurnInputClaimMode::ActiveTurn { turn_id, .. } = value {
            let _ = turn_id;
        }
    });
    // W0294: lash::persistence::TurnInputClaimMode::NextTurn [variant]
    variant_witness(|value: &lash::persistence::TurnInputClaimMode| {
        matches!(value, lash::persistence::TurnInputClaimMode::NextTurn)
    });
    // W0295: lash::process::WaitKind [enum]
    type_witness::<lash::process::WaitKind>();
    // W0296: lash::process::WaitKind::Signal [variant]
    variant_witness(|value: &lash::process::WaitKind| {
        matches!(value, lash::process::WaitKind::Signal { .. })
    });
    // W0297: lash::process::WaitKind::Signal::event_type [field]
    field_witness(|value: &lash::process::WaitKind| {
        let lash::process::WaitKind::Signal { event_type, .. } = value;
        let _ = event_type;
    });
    // W0298: lash::process::WaitKind::Signal::key [field]
    field_witness(|value: &lash::process::WaitKind| {
        let lash::process::WaitKind::Signal { key, .. } = value;
        let _ = key;
    });
    // W0299: lash::process::WaitKind::Signal::name [field]
    field_witness(|value: &lash::process::WaitKind| {
        let lash::process::WaitKind::Signal { name, .. } = value;
        let _ = name;
    });
    // W0300: lash::process::WaitKind::Signal::ordinal [field]
    field_witness(|value: &lash::process::WaitKind| {
        let lash::process::WaitKind::Signal { ordinal, .. } = value;
        let _ = ordinal;
    });
    // W0301: lash::process::WaitState [struct]
    type_witness::<lash::process::WaitState>();
    // W0302: lash::process::WaitState::key [function]
    let _ = lash::process::WaitState::key;
    // W0303: lash::process::WaitState::kind [field]
    field_witness(|value: &lash::process::WaitState| {
        let _ = &value.kind;
    });
    // W0304: lash::process::WaitState::since_ms [field]
    field_witness(|value: &lash::process::WaitState| {
        let _ = &value.since_ms;
    });
    // W0305: lash_core::facade_support::AgentFrameReasonFacadeOps [trait]
    fn trait_witness_0305<T: lash_core::facade_support::AgentFrameReasonFacadeOps>() {}
    // W0306: lash_core::facade_support::BorrowedChronologicalEntry::index [field]
    field_witness(
        |value: &lash_core::facade_support::BorrowedChronologicalEntry| {
            let _ = &value.index;
        },
    );
    // W0307: lash_core::facade_support::BorrowedChronologicalMessage::id [field]
    field_witness(
        |value: &lash_core::facade_support::BorrowedChronologicalMessage| {
            let _ = &value.id;
        },
    );
    // W0308: lash_core::facade_support::JsonSchema::json_schema [function]
    fn meth_0308<T: lash_core::facade_support::JsonSchema>() {
        let _ = T::json_schema;
    }
    // W0309: lash_core::facade_support::JsonSchema::schema_name [function]
    fn meth_0309<T: lash_core::facade_support::JsonSchema>() {
        let _ = T::schema_name;
    }
    // W0310: lash_core::facade_support::ParkedSession::session_id [function]
    let _ = lash_core::facade_support::ParkedSession::session_id;
    // W0311: lash_core::test_support::RuntimeServices::plugins [field]
    field_witness(|value: &lash_core::test_support::RuntimeServices| {
        let _ = &value.plugins;
    });
    // W0312: lash_core::test_support::RuntimeServices::process_env_store [field]
    field_witness(|value: &lash_core::test_support::RuntimeServices| {
        let _ = &value.process_env_store;
    });
    // W0313: lash_core::facade_support::ScopedEffectControllerFacadeOps [trait]
    fn trait_witness_0313<T: lash_core::facade_support::ScopedEffectControllerFacadeOps>() {}
    // W0314: lash_core::facade_support::SelectedQueuedWorkBatchSatisfaction [enum]
    type_witness::<lash_core::facade_support::SelectedQueuedWorkBatchSatisfaction>();
    // W0315: lash_core::facade_support::SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied [variant]
    variant_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkBatchSatisfaction| {
            matches!(
                value,
                lash_core::facade_support::SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied { .. }
            )
        },
    );
    // W0316: lash_core::facade_support::SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied::batch_id [field]
    field_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkBatchSatisfaction| {
            if let lash_core::facade_support::SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied { batch_id, .. } = value { let _ = batch_id; }
        },
    );
    // W0317: lash_core::facade_support::SelectedQueuedWorkBatchSatisfaction::ClaimedNow [variant]
    variant_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkBatchSatisfaction| {
            matches!(
                value,
                lash_core::facade_support::SelectedQueuedWorkBatchSatisfaction::ClaimedNow { .. }
            )
        },
    );
    // W0318: lash_core::facade_support::SelectedQueuedWorkBatchSatisfaction::ClaimedNow::batch_id [field]
    field_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkBatchSatisfaction| {
            if let lash_core::facade_support::SelectedQueuedWorkBatchSatisfaction::ClaimedNow {
                batch_id,
                ..
            } = value
            {
                let _ = batch_id;
            }
        },
    );
    // W0319: lash_core::facade_support::SelectedQueuedWorkDrainError [enum]
    type_witness::<lash_core::facade_support::SelectedQueuedWorkDrainError>();
    // W0320: lash_core::facade_support::SelectedQueuedWorkDrainError::Refused [variant]
    variant_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainError| {
            matches!(
                value,
                lash_core::facade_support::SelectedQueuedWorkDrainError::Refused { .. }
            )
        },
    );
    // W0321: lash_core::facade_support::SelectedQueuedWorkDrainError::Refused::cause [field]
    field_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainError| {
            if let lash_core::facade_support::SelectedQueuedWorkDrainError::Refused {
                cause, ..
            } = value
            {
                let _ = cause;
            }
        },
    );
    // W0322: lash_core::facade_support::SelectedQueuedWorkDrainError::Runtime [variant]
    variant_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainError| {
            matches!(
                value,
                lash_core::facade_support::SelectedQueuedWorkDrainError::Runtime(..)
            )
        },
    );
    // W0323: lash_core::facade_support::SelectedQueuedWorkDrainError::Runtime::0 [field]
    field_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainError| {
            if let lash_core::facade_support::SelectedQueuedWorkDrainError::Runtime(f0) = value {
                let _ = f0;
            }
        },
    );
    // W0324: lash_core::facade_support::SelectedQueuedWorkDrainOutcome::executed_selected_turn [function]
    let _ = lash_core::facade_support::SelectedQueuedWorkDrainOutcome::<()>::executed_selected_turn;
    // W0325: lash_core::facade_support::SelectedQueuedWorkDrainOutcome::expect [function]
    let _ = lash_core::facade_support::SelectedQueuedWorkDrainOutcome::<()>::expect;
    // W0326: lash_core::facade_support::SelectedQueuedWorkDrainOutcome::satisfied [field]
    field_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainOutcome<()>| {
            let _ = &value.satisfied;
        },
    );
    // W0327: lash_core::facade_support::SelectedQueuedWorkDrainOutcome::settled_without_selected_turn [function]
    let _ = lash_core::facade_support::SelectedQueuedWorkDrainOutcome::<()>::settled_without_selected_turn;
    // W0328: lash_core::facade_support::SelectedQueuedWorkDrainOutcome::turn [field]
    field_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainOutcome<()>| {
            let _ = &value.turn;
        },
    );
    // W0329: lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause [enum]
    type_witness::<lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause>();
    // W0330: lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::ExecutionLaneBusy [variant]
    variant_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause| {
            matches!(
                value,
                lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::ExecutionLaneBusy
            )
        },
    );
    // W0331: lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::InterruptedBatchRequiresFullComposition [variant]
    variant_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause| {
            matches!(value, lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::InterruptedBatchRequiresFullComposition{ .. })
        },
    );
    // W0332: lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::InterruptedBatchRequiresFullComposition::required_batch_ids [field]
    field_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause| {
            if let lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::InterruptedBatchRequiresFullComposition { required_batch_ids, .. } = value { let _ = required_batch_ids; }
        },
    );
    // W0333: lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow [variant]
    variant_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause| {
            matches!(value, lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow{ .. })
        },
    );
    // W0334: lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow::batch_enqueue_seq [field]
    field_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause| {
            if let lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow { batch_enqueue_seq, .. } = value { let _ = batch_enqueue_seq; }
        },
    );
    // W0335: lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow::batch_id [field]
    field_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause| {
            if let lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow { batch_id, .. } = value { let _ = batch_id; }
        },
    );
    // W0336: lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow::max_context_tokens [field]
    field_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause| {
            if let lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow { max_context_tokens, .. } = value { let _ = max_context_tokens; }
        },
    );
    // W0337: lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow::required_context_tokens [field]
    field_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause| {
            if let lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow { required_context_tokens, .. } = value { let _ = required_context_tokens; }
        },
    );
    // W0338: lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether [variant]
    variant_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause| {
            matches!(value, lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether{ .. })
        },
    );
    // W0339: lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether::unclaimed_batch_ids [field]
    field_witness(
        |value: &lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause| {
            if let lash_core::facade_support::SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether { unclaimed_batch_ids, .. } = value { let _ = unclaimed_batch_ids; }
        },
    );
    // W0340: lash_core::test_support::SessionObservedProcessOutcome::NoLongerRetained::pruned_at_ms [field]
    field_witness(
        |value: &lash_core::test_support::SessionObservedProcessOutcome| {
            if let lash_core::test_support::SessionObservedProcessOutcome::NoLongerRetained {
                pruned_at_ms,
                ..
            } = value
            {
                let _ = pruned_at_ms;
            }
        },
    );
    // W0341: lash_core::test_support::SessionObservedProcessOutcome::NoLongerRetained::terminal_label [field]
    field_witness(
        |value: &lash_core::test_support::SessionObservedProcessOutcome| {
            if let lash_core::test_support::SessionObservedProcessOutcome::NoLongerRetained {
                terminal_label,
                ..
            } = value
            {
                let _ = terminal_label;
            }
        },
    );
    // W0342: lash_core::test_support::SessionObservedProcessOutcome::NotFound [variant]
    variant_witness(
        |value: &lash_core::test_support::SessionObservedProcessOutcome| {
            matches!(
                value,
                lash_core::test_support::SessionObservedProcessOutcome::NotFound
            )
        },
    );
    // W0343: lash_core::test_support::SessionObservedProcessOutcome::Unavailable::message [field]
    field_witness(
        |value: &lash_core::test_support::SessionObservedProcessOutcome| {
            if let lash_core::test_support::SessionObservedProcessOutcome::Unavailable {
                message,
                ..
            } = value
            {
                let _ = message;
            }
        },
    );
    // W0344: lash_core::facade_support::SessionStreamEvent::Done [variant]
    variant_witness(|value: &lash_core::facade_support::SessionStreamEvent| {
        matches!(value, lash_core::facade_support::SessionStreamEvent::Done)
    });
    // W0345: lash_core::facade_support::SessionStreamEvent::LlmRequest [variant]
    variant_witness(|value: &lash_core::facade_support::SessionStreamEvent| {
        matches!(
            value,
            lash_core::facade_support::SessionStreamEvent::LlmRequest { .. }
        )
    });
    // W0346: lash_core::facade_support::SessionStreamEvent::LlmRequest::message_count [field]
    field_witness(|value: &lash_core::facade_support::SessionStreamEvent| {
        if let lash_core::facade_support::SessionStreamEvent::LlmRequest { message_count, .. } =
            value
        {
            let _ = message_count;
        }
    });
    // W0347: lash_core::facade_support::SessionStreamEvent::LlmRequest::protocol_iteration [field]
    field_witness(|value: &lash_core::facade_support::SessionStreamEvent| {
        if let lash_core::facade_support::SessionStreamEvent::LlmRequest {
            protocol_iteration,
            ..
        } = value
        {
            let _ = protocol_iteration;
        }
    });
    // W0348: lash_core::facade_support::SessionStreamEvent::ReasoningDelta::content [field]
    field_witness(|value: &lash_core::facade_support::SessionStreamEvent| {
        if let lash_core::facade_support::SessionStreamEvent::ReasoningDelta { content, .. } = value
        {
            let _ = content;
        }
    });
    // W0349: lash_core::facade_support::SessionStreamEvent::RetryStatus::attempt [field]
    field_witness(|value: &lash_core::facade_support::SessionStreamEvent| {
        if let lash_core::facade_support::SessionStreamEvent::RetryStatus { attempt, .. } = value {
            let _ = attempt;
        }
    });
    // W0350: lash_core::facade_support::SessionStreamEvent::RetryStatus::envelope [field]
    field_witness(|value: &lash_core::facade_support::SessionStreamEvent| {
        if let lash_core::facade_support::SessionStreamEvent::RetryStatus { envelope, .. } = value {
            let _ = envelope;
        }
    });
    // W0351: lash_core::facade_support::SessionStreamEvent::RetryStatus::max_attempts [field]
    field_witness(|value: &lash_core::facade_support::SessionStreamEvent| {
        if let lash_core::facade_support::SessionStreamEvent::RetryStatus { max_attempts, .. } =
            value
        {
            let _ = max_attempts;
        }
    });
    // W0352: lash_core::facade_support::SessionStreamEvent::RetryStatus::reason [field]
    field_witness(|value: &lash_core::facade_support::SessionStreamEvent| {
        if let lash_core::facade_support::SessionStreamEvent::RetryStatus { reason, .. } = value {
            let _ = reason;
        }
    });
    // W0353: lash_core::facade_support::SessionStreamEvent::RetryStatus::wait_seconds [field]
    field_witness(|value: &lash_core::facade_support::SessionStreamEvent| {
        if let lash_core::facade_support::SessionStreamEvent::RetryStatus { wait_seconds, .. } =
            value
        {
            let _ = wait_seconds;
        }
    });
    // W0354: lash_core::facade_support::SessionStreamEvent::TokenUsage [variant]
    variant_witness(|value: &lash_core::facade_support::SessionStreamEvent| {
        matches!(
            value,
            lash_core::facade_support::SessionStreamEvent::TokenUsage { .. }
        )
    });
    // W0355: lash_core::facade_support::SessionStreamEvent::TokenUsage::cumulative [field]
    field_witness(|value: &lash_core::facade_support::SessionStreamEvent| {
        if let lash_core::facade_support::SessionStreamEvent::TokenUsage { cumulative, .. } = value
        {
            let _ = cumulative;
        }
    });
    // W0356: lash_core::facade_support::SessionStreamEvent::TokenUsage::protocol_iteration [field]
    field_witness(|value: &lash_core::facade_support::SessionStreamEvent| {
        if let lash_core::facade_support::SessionStreamEvent::TokenUsage {
            protocol_iteration,
            ..
        } = value
        {
            let _ = protocol_iteration;
        }
    });
    // W0361: lash_core::facade_support::await_event_coordinator::AwaitEventCoordinator::revoke_session [function]
    fn meth_0361<B: lash_core::facade_support::await_event_coordinator::AwaitEventBackend>() {
        let _ =
            lash_core::facade_support::await_event_coordinator::AwaitEventCoordinator::<B>::revoke_session;
    }
    // W0362: lash_core::facade_support::effect_replay_driver::EffectClaimRequest::lease_ttl_ms [field]
    field_witness(
        |value: &lash_core::facade_support::effect_replay_driver::EffectClaimRequest| {
            let _ = &value.lease_ttl_ms;
        },
    );
    // W0363: lash_core::facade_support::effect_replay_driver::EffectGroupRecord::from_group [function]
    let _ = lash_core::facade_support::effect_replay_driver::EffectGroupRecord::from_group(
        todo!(),
        String::new(),
        None,
        0,
    );
    // W0364: lash_core::facade_support::effect_replay_driver::EffectReplayVocabulary::store_code [function]
    let _ = lash_core::facade_support::effect_replay_driver::EffectReplayVocabulary::store_code;
    // W0365: lash_core::facade_support::effect_replay_driver::EffectRowDefect::UnexpectedPayloads [variant]
    variant_witness(
        |value: &lash_core::facade_support::effect_replay_driver::EffectRowDefect| {
            matches!(value, lash_core::facade_support::effect_replay_driver::EffectRowDefect::UnexpectedPayloads{ .. })
        },
    );
    // W0366: lash_core::facade_support::effect_replay_driver::EffectRowDefect::UnexpectedPayloads::error_json_present [field]
    field_witness(
        |value: &lash_core::facade_support::effect_replay_driver::EffectRowDefect| {
            if let lash_core::facade_support::effect_replay_driver::EffectRowDefect::UnexpectedPayloads { error_json_present, .. } = value { let _ = error_json_present; }
        },
    );
    // W0367: lash_core::facade_support::effect_replay_driver::EffectRowDefect::UnexpectedPayloads::outcome_json_present [field]
    field_witness(
        |value: &lash_core::facade_support::effect_replay_driver::EffectRowDefect| {
            if let lash_core::facade_support::effect_replay_driver::EffectRowDefect::UnexpectedPayloads { outcome_json_present, .. } = value { let _ = outcome_json_present; }
        },
    );
    // W0368: lash_core::facade_support::effect_replay_driver::EffectRowDefect::UnexpectedPayloads::status [field]
    field_witness(
        |value: &lash_core::facade_support::effect_replay_driver::EffectRowDefect| {
            if let lash_core::facade_support::effect_replay_driver::EffectRowDefect::UnexpectedPayloads { status, .. } = value { let _ = status; }
        },
    );
    // W0369: lash_core::facade_support::effect_replay_driver::EffectRowState::Corrupt [variant]
    variant_witness(
        |value: &lash_core::facade_support::effect_replay_driver::EffectRowState| {
            matches!(
                value,
                lash_core::facade_support::effect_replay_driver::EffectRowState::Corrupt(..)
            )
        },
    );
    // W0370: lash_core::facade_support::effect_replay_driver::EffectRowState::Corrupt::0 [field]
    field_witness(
        |value: &lash_core::facade_support::effect_replay_driver::EffectRowState| {
            if let lash_core::facade_support::effect_replay_driver::EffectRowState::Corrupt(f0) =
                value
            {
                let _ = f0;
            }
        },
    );
    // W0371: lash_core::facade_support::effect_replay_driver::EffectRowState::InProgress [variant]
    variant_witness(
        |value: &lash_core::facade_support::effect_replay_driver::EffectRowState| {
            matches!(
                value,
                lash_core::facade_support::effect_replay_driver::EffectRowState::InProgress
            )
        },
    );
    // W0372: lash_core::facade_support::effect_replay_driver::EffectRowState::Settled [variant]
    variant_witness(
        |value: &lash_core::facade_support::effect_replay_driver::EffectRowState| {
            matches!(
                value,
                lash_core::facade_support::effect_replay_driver::EffectRowState::Settled(..)
            )
        },
    );
    // W0373: lash_core::facade_support::effect_replay_driver::EffectRowState::Settled::0 [field]
    field_witness(
        |value: &lash_core::facade_support::effect_replay_driver::EffectRowState| {
            if let lash_core::facade_support::effect_replay_driver::EffectRowState::Settled(f0) =
                value
            {
                let _ = f0;
            }
        },
    );
    // W0374: lash_core::facade_support::effect_replay_driver::decide_effect_claim [function]
    let _ = lash_core::facade_support::effect_replay_driver::decide_effect_claim;
    // W0375: lash_core::facade_support::promise_semantics::PromiseState::Resolved::0 [field]
    field_witness(
        |value: &lash_core::facade_support::promise_semantics::PromiseState| {
            if let lash_core::facade_support::promise_semantics::PromiseState::Resolved(f0) = value
            {
                let _ = f0;
            }
        },
    );
    // W0376: lash_core::facade_support::promise_semantics::PromiseTransition::AlreadyResolved::0 [field]
    field_witness(
        |value: &lash_core::facade_support::promise_semantics::PromiseTransition| {
            if let lash_core::facade_support::promise_semantics::PromiseTransition::AlreadyResolved(f0) = value { let _ = f0; }
        },
    );
    // W0377: lash_core::facade_support::promise_semantics::PromiseTransition::Store::0 [field]
    field_witness(
        |value: &lash_core::facade_support::promise_semantics::PromiseTransition| {
            if let lash_core::facade_support::promise_semantics::PromiseTransition::Store(f0) =
                value
            {
                let _ = f0;
            }
        },
    );
    // W0378: lash_core::facade_support::promise_semantics::resolve [function]
    let _ = lash_core::facade_support::promise_semantics::resolve;
    // W0379: lash_core::facade_support::refuse_unhonored_group_membership [function]
    let _ = lash_core::facade_support::refuse_unhonored_group_membership;
    // W0380: lash_core::facade_support::validate_replayed_effect_envelope [function]
    let _ = lash_core::facade_support::validate_replayed_effect_envelope;
    // W0381: lash::runtime::OutputState [enum]
    type_witness::<lash::runtime::OutputState>();
    // W0382: lash::runtime::OutputState::EmptyOutput [variant]
    variant_witness(|value: &lash::runtime::OutputState| {
        matches!(value, lash::runtime::OutputState::EmptyOutput)
    });
    // W0383: lash::runtime::OutputState::RecoveredFromError [variant]
    variant_witness(|value: &lash::runtime::OutputState| {
        matches!(value, lash::runtime::OutputState::RecoveredFromError)
    });
    // W0384: lash::runtime::OutputState::TracebackOnly [variant]
    variant_witness(|value: &lash::runtime::OutputState| {
        matches!(value, lash::runtime::OutputState::TracebackOnly)
    });
    // W0385: lash::runtime::OutputState::Usable [variant]
    variant_witness(|value: &lash::runtime::OutputState| {
        matches!(value, lash::runtime::OutputState::Usable)
    });
    // W0386: lash::durability::RuntimeReplay [struct]
    type_witness::<lash::durability::RuntimeReplay>();
    // W0387: lash::durability::RuntimeReplay::attribution [field]
    field_witness(|value: &lash::durability::RuntimeReplay| {
        let _ = &value.attribution;
    });
    // W0388: lash::durability::RuntimeReplay::key [field]
    field_witness(|value: &lash::durability::RuntimeReplay| {
        let _ = &value.key;
    });
    // W0389: lash::durability::RuntimeReplayAttribution [enum]
    type_witness::<lash::durability::RuntimeReplayAttribution>();
    // W0390: lash::durability::RuntimeReplayAttribution::ToolIntent [variant]
    variant_witness(|value: &lash::durability::RuntimeReplayAttribution| {
        matches!(
            value,
            lash::durability::RuntimeReplayAttribution::ToolIntent(..)
        )
    });
    // W0391: lash::durability::RuntimeReplayAttribution::ToolIntent::0 [field]
    field_witness(|value: &lash::durability::RuntimeReplayAttribution| {
        let lash::durability::RuntimeReplayAttribution::ToolIntent(f0) = value;
        let _ = f0;
    });
    // W0392: lash::durability::RuntimeSubject [enum]
    type_witness::<lash::durability::RuntimeSubject>();
    // W0393: lash::durability::RuntimeSubject::Effect [variant]
    variant_witness(|value: &lash::durability::RuntimeSubject| {
        matches!(value, lash::durability::RuntimeSubject::Effect { .. })
    });
    // W0394: lash::durability::RuntimeSubject::Effect::effect_id [field]
    field_witness(|value: &lash::durability::RuntimeSubject| {
        if let lash::durability::RuntimeSubject::Effect { effect_id, .. } = value {
            let _ = effect_id;
        }
    });
    // W0396: lash::durability::RuntimeSubject::Process [variant]
    variant_witness(|value: &lash::durability::RuntimeSubject| {
        matches!(value, lash::durability::RuntimeSubject::Process { .. })
    });
    // W0397: lash::durability::RuntimeSubject::Process::process_id [field]
    field_witness(|value: &lash::durability::RuntimeSubject| {
        if let lash::durability::RuntimeSubject::Process { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // W0398: lash::durability::RuntimeSubject::ProcessEvent [variant]
    variant_witness(|value: &lash::durability::RuntimeSubject| {
        matches!(value, lash::durability::RuntimeSubject::ProcessEvent { .. })
    });
    // W0399: lash::durability::RuntimeSubject::ProcessEvent::event_type [field]
    field_witness(|value: &lash::durability::RuntimeSubject| {
        if let lash::durability::RuntimeSubject::ProcessEvent { event_type, .. } = value {
            let _ = event_type;
        }
    });
    // W0400: lash::durability::RuntimeSubject::ProcessEvent::process_id [field]
    field_witness(|value: &lash::durability::RuntimeSubject| {
        if let lash::durability::RuntimeSubject::ProcessEvent { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // W0401: lash::durability::RuntimeSubject::ProcessEvent::sequence [field]
    field_witness(|value: &lash::durability::RuntimeSubject| {
        if let lash::durability::RuntimeSubject::ProcessEvent { sequence, .. } = value {
            let _ = sequence;
        }
    });
    // W0402: lash::durability::RuntimeSubject::SessionNode [variant]
    variant_witness(|value: &lash::durability::RuntimeSubject| {
        matches!(value, lash::durability::RuntimeSubject::SessionNode { .. })
    });
    // W0403: lash::durability::RuntimeSubject::SessionNode::node_id [field]
    field_witness(|value: &lash::durability::RuntimeSubject| {
        if let lash::durability::RuntimeSubject::SessionNode { node_id, .. } = value {
            let _ = node_id;
        }
    });
    // W0404: lash::durability::RuntimeSubject::TriggerOccurrence [variant]
    variant_witness(|value: &lash::durability::RuntimeSubject| {
        matches!(
            value,
            lash::durability::RuntimeSubject::TriggerOccurrence { .. }
        )
    });
    // W0405: lash::durability::RuntimeSubject::TriggerOccurrence::occurrence_id [field]
    field_witness(|value: &lash::durability::RuntimeSubject| {
        if let lash::durability::RuntimeSubject::TriggerOccurrence { occurrence_id, .. } = value {
            let _ = occurrence_id;
        }
    });
    // W0406: lash::durability::ProcessLocalExecution [struct]
    type_witness::<lash::durability::ProcessLocalExecution>();
    // W0408: lash::durability::ProcessLocalExecution::registry [field]
    field_witness(|value: &lash::durability::ProcessLocalExecution| {
        let _ = &value.registry;
    });
    // W0409: lash::durability::ProcessLocalExecution::turn_cancellation [field]
    field_witness(|value: &lash::durability::ProcessLocalExecution| {
        let _ = &value.turn_cancellation;
    });
    // W0410: lash::durability::ProcessTurnCancellation [struct]
    type_witness::<lash::durability::ProcessTurnCancellation>();
    // W0411: lash::durability::ProcessTurnCancellation::cancellation [field]
    field_witness(|value: &lash::durability::ProcessTurnCancellation| {
        let _ = &value.cancellation;
    });
    // W0412: lash::durability::ProcessTurnCancellation::new [function]
    let _ = lash::durability::ProcessTurnCancellation::new;
    // W0413: lash::durability::ProcessTurnCancellation::scope [field]
    field_witness(|value: &lash::durability::ProcessTurnCancellation| {
        let _ = &value.scope;
    });
    // W0414: lash::durability::RuntimeAwaitEventOptions [struct]
    type_witness::<lash::durability::RuntimeAwaitEventOptions>();
    // W0415: lash::durability::RuntimeAwaitEventOptions::cancellation [field]
    field_witness(|value: &lash::durability::RuntimeAwaitEventOptions| {
        let _ = &value.cancellation;
    });
    // W0416: lash::durability::RuntimeAwaitEventOptions::clock [field]
    field_witness(|value: &lash::durability::RuntimeAwaitEventOptions| {
        let _ = &value.clock;
    });
    // W0417: lash::durability::RuntimeAwaitEventOptions::deadline [field]
    field_witness(|value: &lash::durability::RuntimeAwaitEventOptions| {
        let _ = &value.deadline;
    });
    // W0418: lash::durability::RuntimeAwaitEventOptions::observe_turn_cancel [field]
    field_witness(|value: &lash::durability::RuntimeAwaitEventOptions| {
        let _ = &value.observe_turn_cancel;
    });
    // W0419: lash::durability::RuntimeAwaitEventOptions::turn_cancel_scope [field]
    field_witness(|value: &lash::durability::RuntimeAwaitEventOptions| {
        let _ = &value.turn_cancel_scope;
    });
    // W0420: lash::durability::RuntimeSleepOptions [struct]
    type_witness::<lash::durability::RuntimeSleepOptions>();
    // W0421: lash::durability::RuntimeSleepOptions::cancellation [field]
    field_witness(|value: &lash::durability::RuntimeSleepOptions| {
        let _ = &value.cancellation;
    });
    // W0422: lash::durability::RuntimeSleepOptions::observe_turn_cancel [field]
    field_witness(|value: &lash::durability::RuntimeSleepOptions| {
        let _ = &value.observe_turn_cancel;
    });
    // W0423: lash::durability::RuntimeSleepOptions::turn_cancel_scope [field]
    field_witness(|value: &lash::durability::RuntimeSleepOptions| {
        let _ = &value.turn_cancel_scope;
    });
    // W0424: lash::durability::EffectJournalIdentity [struct]
    type_witness::<lash::durability::EffectJournalIdentity>();
    // W0425: lash::durability::EffectJournalIdentity::key [function]
    let _ = lash::durability::EffectJournalIdentity::key;
    // W0426: lash::durability::EffectJournalIdentity::session_id [function]
    let _ = lash::durability::EffectJournalIdentity::session_id;
    // W0427: lash::durability::TriggerLocalExecution [struct]
    type_witness::<lash::durability::TriggerLocalExecution>();
    // W0428: lash::durability::TriggerLocalExecution::store [field]
    field_witness(|value: &lash::durability::TriggerLocalExecution| {
        let _ = &value.store;
    });
    // W0429: lash::durability::CanonicalRuntimeEffectEnvelope [struct]
    type_witness::<lash::durability::CanonicalRuntimeEffectEnvelope>();
    // W0430: lash::durability::CanonicalRuntimeEffectEnvelope::hash [function]
    let _ = lash::durability::CanonicalRuntimeEffectEnvelope::hash;
    // W0431: lash::durability::RuntimeEffectReplayTrace [struct]
    type_witness::<lash::durability::RuntimeEffectReplayTrace>();
    // W0432: lash::runtime::RuntimeControlConfig [struct]
    type_witness::<lash::runtime::RuntimeControlConfig>();
    // W0433: lash::runtime::RuntimeControlConfig::effect_host [field]
    field_witness(|value: &lash::runtime::RuntimeControlConfig| {
        let _ = &value.effect_host;
    });
    // W0434: lash::runtime::RuntimeControlConfig::lease_timings [field]
    field_witness(|value: &lash::runtime::RuntimeControlConfig| {
        let _ = &value.lease_timings;
    });
    // W0436: lash::runtime::RuntimeControlConfig::process_tool_visibility_filter [field]
    field_witness(|value: &lash::runtime::RuntimeControlConfig| {
        let _ = &value.process_tool_visibility_filter;
    });
    // W0437: lash::runtime::RuntimeControlConfig::termination [field]
    field_witness(|value: &lash::runtime::RuntimeControlConfig| {
        let _ = &value.termination;
    });
    // W0438: lash::runtime::RuntimeDurabilityConfig [struct]
    type_witness::<lash::runtime::RuntimeDurabilityConfig>();
    // W0439: lash::runtime::RuntimeDurabilityConfig::attachment_store [field]
    field_witness(|value: &lash::runtime::RuntimeDurabilityConfig| {
        let _ = &value.attachment_store;
    });
    // W0440: lash::runtime::RuntimeDurabilityConfig::process_env_store [field]
    field_witness(|value: &lash::runtime::RuntimeDurabilityConfig| {
        let _ = &value.process_env_store;
    });
    // W0441: lash::runtime::RuntimePromptConfig [struct]
    type_witness::<lash::runtime::RuntimePromptConfig>();
    // W0442: lash::runtime::RuntimePromptConfig::prompt [field]
    field_witness(|value: &lash::runtime::RuntimePromptConfig| {
        let _ = &value.prompt;
    });
    // W0443: lash::runtime::RuntimeProviderConfig [struct]
    type_witness::<lash::runtime::RuntimeProviderConfig>();
    // W0444: lash::runtime::RuntimeProviderConfig::provider_resolver [field]
    field_witness(|value: &lash::runtime::RuntimeProviderConfig| {
        let _ = &value.provider_resolver;
    });
    // W0445: lash::runtime::RuntimeTracingConfig [struct]
    type_witness::<lash::runtime::RuntimeTracingConfig>();
    // W0446: lash::runtime::RuntimeTracingConfig::trace_context [field]
    field_witness(|value: &lash::runtime::RuntimeTracingConfig| {
        let _ = &value.trace_context;
    });
    // W0447: lash::runtime::RuntimeTracingConfig::trace_level [field]
    field_witness(|value: &lash::runtime::RuntimeTracingConfig| {
        let _ = &value.trace_level;
    });
    // W0448: lash::runtime::RuntimeTracingConfig::trace_sink [field]
    field_witness(|value: &lash::runtime::RuntimeTracingConfig| {
        let _ = &value.trace_sink;
    });
    // W0449: lash::tools::ToolInvocation [struct]
    type_witness::<lash::tools::ToolInvocation>();
    // W0450: lash::tools::ToolInvocation::args [field]
    field_witness(|value: &lash::tools::ToolInvocation| {
        let _ = &value.args;
    });
    // W0451: lash::tools::ToolInvocation::child_execution_trace_hook [field]
    field_witness(|value: &lash::tools::ToolInvocation| {
        let _ = &value.child_execution_trace_hook;
    });
    // W0452: lash::tools::ToolInvocation::execution_grant [field]
    field_witness(|value: &lash::tools::ToolInvocation| {
        let _ = &value.execution_grant;
    });
    // W0453: lash::tools::ToolInvocation::id [field]
    field_witness(|value: &lash::tools::ToolInvocation| {
        let _ = &value.id;
    });
    // W0454: lash::tools::ToolInvocation::new [function]
    let _ = lash::tools::ToolInvocation::new(String::new(), todo!(), todo!());
    // W0455: lash::tools::ToolInvocation::tool_id [field]
    field_witness(|value: &lash::tools::ToolInvocation| {
        let _ = &value.tool_id;
    });
    // W0456: lash::tools::ToolInvocation::with_child_execution_trace_hook [function]
    let _ = lash::tools::ToolInvocation::with_child_execution_trace_hook;
    // W0457: lash::tools::ToolInvocation::with_execution_grant [function]
    let _ = lash::tools::ToolInvocation::with_execution_grant;
    // W0458: lash::tools::ToolBatchReplies [struct]
    type_witness::<lash::tools::ToolBatchReplies>();
    // W0459: lash::tools::ToolBatchReplies::replies [field]
    field_witness(|value: &lash::tools::ToolBatchReplies| {
        let _ = &value.replies;
    });
    // W0460: lash::tools::ToolBatchReplies::settlement_order [field]
    field_witness(|value: &lash::tools::ToolBatchReplies| {
        let _ = &value.settlement_order;
    });
    // W0461: lash::tools::ToolInvocationReply [struct]
    type_witness::<lash::tools::ToolInvocationReply>();
    // W0462: lash::tools::ToolInvocationReply::cancelled [function]
    let _ = lash::tools::ToolInvocationReply::cancelled(String::new());
    // W0463: lash::tools::ToolInvocationReply::error [function]
    let _ = lash::tools::ToolInvocationReply::error;
    // W0464: lash::tools::ToolInvocationReply::from_output [function]
    let _ = lash::tools::ToolInvocationReply::from_output;
    // W0465: lash::tools::ToolInvocationReply::output [field]
    field_witness(|value: &lash::tools::ToolInvocationReply| {
        let _ = &value.output;
    });
    // W0466: lash::tools::ToolInvocationReply::record [field]
    field_witness(|value: &lash::tools::ToolInvocationReply| {
        let _ = &value.record;
    });
    // W0467: lash::tools::ToolInvocationReply::success [function]
    let _ = lash::tools::ToolInvocationReply::success;
    // W0468: lash::messages::SessionMessageTreeNode [struct]
    type_witness::<lash::messages::SessionMessageTreeNode>();
    // W0469: lash::messages::SessionMessageTreeNode::active [field]
    field_witness(|value: &lash::messages::SessionMessageTreeNode| {
        let _ = &value.active;
    });
    // W0470: lash::messages::SessionMessageTreeNode::node_id [field]
    field_witness(|value: &lash::messages::SessionMessageTreeNode| {
        let _ = &value.node_id;
    });
    // W0471: lash::messages::SessionMessageTreeNode::parent_message_node_id [field]
    field_witness(|value: &lash::messages::SessionMessageTreeNode| {
        let _ = &value.parent_message_node_id;
    });
    // W0472: lash::messages::SessionMessageTreeNode::timestamp [field]
    field_witness(|value: &lash::messages::SessionMessageTreeNode| {
        let _ = &value.timestamp;
    });
    // W0473: lash::messages::SharedJsonValue [struct]
    type_witness::<lash::messages::SharedJsonValue>();
    // W0474: lash::messages::SharedJsonValue::0 [field]
    field_witness(|value: &lash::messages::SharedJsonValue| {
        let _ = &value.0;
    });
    // W0475: lash::messages::SharedJsonValue::new [function]
    let _ = lash::messages::SharedJsonValue::new;
    // W0476: lash::messages::SharedJsonValue::to_owned [function]
    let _ = lash::messages::SharedJsonValue::to_owned;
    // W0477: lash::sync::LockResultExt [trait]
    fn trait_witness_0477<T: lash::sync::LockResultExt<()>>() {}
    // W0478: lash::sync::LockResultExt::recover [function]
    fn meth_0478<T: lash::sync::LockResultExt<()>>(_: &T) {
        let _ = T::recover;
    }
    // W0479: lash::sync::MutexExt::try_lock_recover [function]
    fn meth_0479<T: lash::sync::MutexExt<()>>(_: &T) {
        let _ = T::try_lock_recover;
    }
    // W0480: lash::runtime::ApplyConfigPatch [struct]
    type_witness::<lash::runtime::ApplyConfigPatch>();
    // W0481: lash::durability::ProcessLocalExecution::effect_controller [field]
    field_witness(|value: &lash::durability::ProcessLocalExecution| {
        let _ = &value.effect_controller;
    });
    // W0482: lash::runtime::LashRuntime::from_environment_with_plugin_options [function]
    let _ = lash::runtime::LashRuntime::from_environment_with_plugin_options;
    // W0483: lash::SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow::batch_enqueue_seq [field]
    field_witness(|value: &lash::SelectedQueuedWorkDrainRefusalCause| {
        if let lash::SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow {
            batch_enqueue_seq,
            ..
        } = value
        {
            let _ = batch_enqueue_seq;
        }
    });
    // W0484: lash::runtime::RuntimeErrorCode::RuntimeEffectGroupDrainDeferred [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::RuntimeEffectGroupDrainDeferred
        )
    });
    // W0485: lash::runtime::ExecutionScope::from_journal_key [function]
    let _ = lash::runtime::ExecutionScope::from_journal_key;
    // W0487: lash_core::GroupDrainReport [struct]
    type_witness::<lash_core::GroupDrainReport>();
    // W0488: lash_core::GroupDrainReport::group_key [field]
    field_witness(|value: &lash_core::GroupDrainReport| {
        let _ = &value.group_key;
    });
    // W0489: lash_core::GroupDrainReport::disposition [field]
    field_witness(|value: &lash_core::GroupDrainReport| {
        let _ = &value.disposition;
    });
    // W0490: lash_core::GroupDrainReport::children [field]
    field_witness(|value: &lash_core::GroupDrainReport| {
        let _ = &value.children;
    });
    // W0491: lash_core::GroupDrainReport::settled [function]
    let _ = lash_core::GroupDrainReport::settled;
    // W0492: lash_core::GroupDrainReport::is_complete [function]
    let _ = lash_core::GroupDrainReport::is_complete;
    // W0493: lash_core::DrainedChild [struct]
    type_witness::<lash_core::DrainedChild>();
    // W0494: lash_core::DrainedChild::replay_key [field]
    field_witness(|value: &lash_core::DrainedChild| {
        let _ = &value.replay_key;
    });
    // W0495: lash_core::DrainedChild::outcome [field]
    field_witness(|value: &lash_core::DrainedChild| {
        let _ = &value.outcome;
    });
    // W0496: lash_core::ChildDrainOutcome [enum]
    type_witness::<lash_core::ChildDrainOutcome>();
    // W0497: lash_core::ChildDrainOutcome::Settled [variant]
    variant_witness(|value: &lash_core::ChildDrainOutcome| {
        matches!(value, lash_core::ChildDrainOutcome::Settled)
    });
    // W0498: lash_core::ChildDrainOutcome::Contested [variant]
    variant_witness(|value: &lash_core::ChildDrainOutcome| {
        matches!(value, lash_core::ChildDrainOutcome::Contested)
    });
    // W0499: lash_core::ChildDrainOutcome::LeaseLive [variant]
    variant_witness(|value: &lash_core::ChildDrainOutcome| {
        matches!(value, lash_core::ChildDrainOutcome::LeaseLive { .. })
    });
    // W0500: lash_core::ChildDrainOutcome::LeaseLive::expires_at_ms [field]
    field_witness(|value: &lash_core::ChildDrainOutcome| {
        if let lash_core::ChildDrainOutcome::LeaseLive { expires_at_ms, .. } = value {
            let _ = expires_at_ms;
        }
    });
    // W0502: lash_core::ChildDrainOutcome::Interrupted [variant]
    variant_witness(|value: &lash_core::ChildDrainOutcome| {
        matches!(value, lash_core::ChildDrainOutcome::Interrupted)
    });
    // W0503: lash_core::ChildDrainOutcome::Corrupt [variant]
    variant_witness(|value: &lash_core::ChildDrainOutcome| {
        matches!(value, lash_core::ChildDrainOutcome::Corrupt { .. })
    });
    // W0504: lash_core::ChildDrainOutcome::Corrupt::status [field]
    field_witness(|value: &lash_core::ChildDrainOutcome| {
        if let lash_core::ChildDrainOutcome::Corrupt { status, .. } = value {
            let _ = status;
        }
    });
    // W0505: lash_core::ChildDrainOutcome::NoExecutor [variant]
    variant_witness(|value: &lash_core::ChildDrainOutcome| {
        matches!(value, lash_core::ChildDrainOutcome::NoExecutor)
    });
    // W0506: lash::durability::CanonicalRuntimeEffectEnvelope::json [function]
    let _ = lash::durability::CanonicalRuntimeEffectEnvelope::json;
    // W0508: lash::QueuedTurnDrain::expect [function]
    let _ = lash::QueuedTurnDrain::<()>::expect;
    // W0509: lash::QueuedWorkClaimRefusal::as_str [function]
    let _ = lash::QueuedWorkClaimRefusal::as_str;
    // W0510: lash::runtime::AssembledTurn::turn_cancel_input_outcome [field]
    field_witness(|value: &lash::runtime::AssembledTurn| {
        let _ = &value.turn_cancel_input_outcome;
    });
    // W0512: lash::testing::MockSessionManager::created [field]
    field_witness(|value: &lash::testing::MockSessionManager| {
        let _ = &value.created;
    });
    // W0513: lash::testing::MockSessionManager::created_snapshot [function]
    let _ = lash::testing::MockSessionManager::created_snapshot;
    // W0514: lash::testing::MockSessionManager::process_registry [field]
    field_witness(|value: &lash::testing::MockSessionManager| {
        let _ = &value.process_registry;
    });
    // W0515: lash::testing::MockSessionManager::snapshot [field]
    field_witness(|value: &lash::testing::MockSessionManager| {
        let _ = &value.snapshot;
    });
    // W0516: lash::testing::MockSessionManager::tool_catalog [field]
    field_witness(|value: &lash::testing::MockSessionManager| {
        let _ = &value.tool_catalog;
    });
    // W0517: lash::testing::MockSessionManager::tool_registry [field]
    field_witness(|value: &lash::testing::MockSessionManager| {
        let _ = &value.tool_registry;
    });
    // W0518: lash::testing::MockSessionManager::turn [field]
    field_witness(|value: &lash::testing::MockSessionManager| {
        let _ = &value.turn;
    });
    // W0520: lash::testing::MockSessionManager::with_snapshot [function]
    let _ = lash::testing::MockSessionManager::with_snapshot;
    // W0521: lash::testing::MockSessionManager::with_tool_catalog [function]
    let _ = lash::testing::MockSessionManager::with_tool_catalog;
    // W0522: lash::testing::MockSessionManager::with_tool_registry [function]
    let _ = lash::testing::MockSessionManager::with_tool_registry;
    // W0523: lash::testing::MockSessionManager::with_turn [function]
    let _ = lash::testing::MockSessionManager::with_turn;
    // W0524: lash::testing::TestClock::advance [function]
    let _ = lash::testing::TestClock::advance;
    // W0525: lash::testing::TestClock::set [function]
    let _ = lash::testing::TestClock::set;
}
