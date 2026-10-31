//! Compile-time witnesses for process-area facade and integrator contracts.
//!
//! Continuation of `processes_evidence.rs`; see that file for the FIG-2106
//! slice accounting.

#![cfg(feature = "testing")]
#![allow(dead_code, unreachable_code, unused_variables, unused_imports)]
#![allow(clippy::all)]

fn type_witness<T>() {}
fn member_witness<T>(_: T) {}
fn field_witness<T>(_: impl FnOnce(&T)) {}
fn variant_witness<T>(_: impl FnOnce(&T) -> bool) {}

fn processes_area_witnesses_b() {
    // W0345: lash::process::SessionScopeId [struct]
    type_witness::<lash::process::SessionScopeId>();
    // W0346: lash::process::SessionScopeId::as_str [function]
    let _ = lash::process::SessionScopeId::as_str;
    // W0347: lash::process::SessionScopeId::new [function]
    let _ = lash::process::SessionScopeId::new(String::new());
    // W0358: lash::runtime::ExecutionScope::Process [variant]
    variant_witness(|value: &lash::runtime::ExecutionScope| {
        matches!(value, lash::runtime::ExecutionScope::Process { .. })
    });
    // W0359: lash::runtime::ExecutionScope::Process::process_id [field]
    field_witness(|value: &lash::runtime::ExecutionScope| {
        if let lash::runtime::ExecutionScope::Process { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // W0360: lash::runtime::ExecutionScope::process [function]
    let _ =
        lash::runtime::ExecutionScope::process(lash_vocabulary::identity::ProcessId::fixture("x"));
    // W0361: lash::runtime::ProcessCommand::Await [variant]
    variant_witness(|value: &lash::runtime::ProcessCommand| {
        matches!(value, lash::runtime::ProcessCommand::Await { .. })
    });
    // W0363: lash::runtime::ProcessCommand::Cancel [variant]
    variant_witness(|value: &lash::runtime::ProcessCommand| {
        matches!(value, lash::runtime::ProcessCommand::Cancel { .. })
    });
    // W0366: lash::runtime::ProcessCommand::DeleteSession [variant]
    variant_witness(|value: &lash::runtime::ProcessCommand| {
        matches!(value, lash::runtime::ProcessCommand::DeleteSession { .. })
    });
    // W0367: lash::runtime::ProcessCommand::DeleteSession::session_id [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::DeleteSession { session_id, .. } = value {
            let _ = session_id;
        }
    });
    // W0368: lash::runtime::ProcessCommand::List [variant]
    variant_witness(|value: &lash::runtime::ProcessCommand| {
        matches!(value, lash::runtime::ProcessCommand::List { .. })
    });
    // W0369: lash::runtime::ProcessCommand::List::selection [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::List { selection } = value {
            let _ = selection;
        }
    });
    // W0376: lash::runtime::ProcessCommand::Start::execution_context [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::Start {
            execution_context, ..
        } = value
        {
            let _ = execution_context;
        }
    });
    // W0377: lash::runtime::ProcessCommand::Start::observers [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::Start { observers, .. } = value {
            let _ = observers;
        }
    });
    // W0378: lash::runtime::ProcessCommand::Start::registration [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::Start { registration, .. } = value {
            let _ = registration;
        }
    });
    // W0379: lash::runtime::ProcessCommand::Transfer [variant]
    variant_witness(|value: &lash::runtime::ProcessCommand| {
        matches!(value, lash::runtime::ProcessCommand::Transfer { .. })
    });
    // W0380: lash::runtime::ProcessCommand::Transfer::from_scope [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::Transfer { from_scope, .. } = value {
            let _ = from_scope;
        }
    });
    // W0381: lash::runtime::ProcessCommand::Transfer::process_ids [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::Transfer { process_ids, .. } = value {
            let _ = process_ids;
        }
    });
    // W0382: lash::runtime::ProcessCommand::Transfer::to_scope [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::Transfer { to_scope, .. } = value {
            let _ = to_scope;
        }
    });
    // W0383: lash::runtime::ProcessCommand::effect_id [function]
    let _ = lash::runtime::ProcessCommand::effect_id;
    // W0384: lash::runtime::ProcessEffectOutcome [enum]
    type_witness::<lash::runtime::ProcessEffectOutcome>();
    // W0385: lash::runtime::ProcessEffectOutcome::Await [variant]
    variant_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        matches!(value, lash::runtime::ProcessEffectOutcome::Await { .. })
    });
    // W0386: lash::runtime::ProcessEffectOutcome::Await::output [field]
    field_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        if let lash::runtime::ProcessEffectOutcome::Await { output, .. } = value {
            let _ = output;
        }
    });
    // W0387: lash::runtime::ProcessEffectOutcome::Cancel [variant]
    variant_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        matches!(value, lash::runtime::ProcessEffectOutcome::Cancel { .. })
    });
    // W0388: lash::runtime::ProcessEffectOutcome::Cancel::record [field]
    field_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        if let lash::runtime::ProcessEffectOutcome::Cancel { record, .. } = value {
            let _ = record;
        }
    });
    // W0389: lash::runtime::ProcessEffectOutcome::DeleteSession [variant]
    variant_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        matches!(
            value,
            lash::runtime::ProcessEffectOutcome::DeleteSession { .. }
        )
    });
    // W0390: lash::runtime::ProcessEffectOutcome::DeleteSession::report [field]
    field_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        if let lash::runtime::ProcessEffectOutcome::DeleteSession { report, .. } = value {
            let _ = report;
        }
    });
    // W0391: lash::runtime::ProcessEffectOutcome::List [variant]
    variant_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        matches!(value, lash::runtime::ProcessEffectOutcome::List { .. })
    });
    // W0392: lash::runtime::ProcessEffectOutcome::List::entries [field]
    field_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        if let lash::runtime::ProcessEffectOutcome::List { entries, .. } = value {
            let _ = entries;
        }
    });
    // W0395: lash::runtime::ProcessEffectOutcome::Start [variant]
    variant_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        matches!(value, lash::runtime::ProcessEffectOutcome::Start { .. })
    });
    // W0396: lash::runtime::ProcessEffectOutcome::Start::record [field]
    field_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        if let lash::runtime::ProcessEffectOutcome::Start { record, .. } = value {
            let _ = record;
        }
    });
    // W0397: lash::runtime::ProcessEffectOutcome::Transfer [variant]
    variant_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        matches!(value, lash::runtime::ProcessEffectOutcome::Transfer)
    });
    // W0402: lash::runtime::RuntimeEffectCommand::Process::command [field]
    field_witness(|value: &lash::runtime::RuntimeEffectCommand| {
        if let lash::runtime::RuntimeEffectCommand::Process { command, .. } = value {
            let _ = command;
        }
    });
    // W0403: lash::runtime::RuntimeEffectCommand::process [function]
    let _ = lash::runtime::RuntimeEffectCommand::process;
    // W0404: lash::runtime::RuntimeEffectKind::Process [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectKind| {
        matches!(value, lash::runtime::RuntimeEffectKind::Process)
    });
    // W0412: lash::runtime::RuntimeEffectOutcome::Process [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        matches!(value, lash::runtime::RuntimeEffectOutcome::Process { .. })
    });
    // W0413: lash::runtime::RuntimeEffectOutcome::Process::result [field]
    field_witness(|value: &lash::runtime::RuntimeEffectOutcome| {
        if let lash::runtime::RuntimeEffectOutcome::Process { result, .. } = value {
            let _ = result;
        }
    });
    // W0415: lash::runtime::RuntimeEffectOutcome::into_process [function]
    let _ = lash::runtime::RuntimeEffectOutcome::into_process;
    // W0416: lash::runtime::RuntimeError::missing_process_execution_id [function]
    let _ = lash::runtime::RuntimeError::missing_process_execution_id;
    // W0417: lash::runtime::RuntimeErrorCode::MissingProcessExecutionId [variant]
    variant_witness(|value: &lash::runtime::RuntimeErrorCode| {
        matches!(
            value,
            lash::runtime::RuntimeErrorCode::MissingProcessExecutionId
        )
    });
    // W0434: lash::durability::EffectJournalRetirement::Process [variant]
    variant_witness(|value: &lash::durability::EffectJournalRetirement| {
        matches!(
            value,
            lash::durability::EffectJournalRetirement::Process { .. }
        )
    });
    // W0435: lash::durability::EffectJournalRetirement::Process::process_id [field]
    field_witness(|value: &lash::durability::EffectJournalRetirement| {
        if let lash::durability::EffectJournalRetirement::Process { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // W0436: lash::durability::EffectJournalRetirement::process [function]
    let _ = lash::durability::EffectJournalRetirement::process(
        lash_vocabulary::identity::ProcessId::fixture("x"),
    );
    // W0437: lash::persistence::ProcessChange [struct]
    type_witness::<lash::persistence::ProcessChange>();
    // W0438: lash::persistence::ProcessChangeKind::Deleted [variant]
    variant_witness(|value: &lash::persistence::ProcessChangeKind| {
        matches!(value, lash::persistence::ProcessChangeKind::Deleted { .. })
    });
    // W0439: lash::persistence::ProcessChangeKind::Deleted::tombstone [field]
    field_witness(|value: &lash::persistence::ProcessChangeKind| {
        if let lash::persistence::ProcessChangeKind::Deleted { tombstone, .. } = value {
            let _ = tombstone;
        }
    });
    // W0440: lash::persistence::ProcessChangeKind::Upsert [variant]
    variant_witness(|value: &lash::persistence::ProcessChangeKind| {
        matches!(value, lash::persistence::ProcessChangeKind::Upsert { .. })
    });
    // W0441: lash::persistence::ProcessChangeKind::Upsert::record [field]
    field_witness(|value: &lash::persistence::ProcessChangeKind| {
        if let lash::persistence::ProcessChangeKind::Upsert { record, .. } = value {
            let _ = record;
        }
    });
    field_witness(|value: &lash::persistence::ProcessChange| {
        let _: lash::process::ProcessChangeCursor = value.cursor;
        let _: &lash::persistence::ProcessChangeKind = &value.kind;
    });
    // W0442: lash::persistence::ProcessCompletionOutcome [enum]
    type_witness::<lash::persistence::ProcessCompletionOutcome>();
    // W0443: lash::persistence::ProcessCompletionOutcome::AlreadyApplied [variant]
    variant_witness(|value: &lash::persistence::ProcessCompletionOutcome| {
        matches!(
            value,
            lash::persistence::ProcessCompletionOutcome::AlreadyApplied { .. }
        )
    });
    // W0444: lash::persistence::ProcessCompletionOutcome::AlreadyApplied::stored [field]
    field_witness(|value: &lash::persistence::ProcessCompletionOutcome| {
        if let lash::persistence::ProcessCompletionOutcome::AlreadyApplied { stored, .. } = value {
            let _ = stored;
        }
    });
    // W0445: lash::persistence::ProcessCompletionOutcome::Committed [variant]
    variant_witness(|value: &lash::persistence::ProcessCompletionOutcome| {
        matches!(
            value,
            lash::persistence::ProcessCompletionOutcome::Committed(..)
        )
    });
    // W0446: lash::persistence::ProcessCompletionOutcome::Committed::0 [field]
    field_witness(|value: &lash::persistence::ProcessCompletionOutcome| {
        if let lash::persistence::ProcessCompletionOutcome::Committed(f0) = value {
            let _ = f0;
        }
    });
    // W0447: lash::persistence::ProcessCompletionOutcome::Superseded [variant]
    variant_witness(|value: &lash::persistence::ProcessCompletionOutcome| {
        matches!(
            value,
            lash::persistence::ProcessCompletionOutcome::Superseded { .. }
        )
    });
    // W0448: lash::persistence::ProcessCompletionOutcome::Superseded::stored [field]
    field_witness(|value: &lash::persistence::ProcessCompletionOutcome| {
        if let lash::persistence::ProcessCompletionOutcome::Superseded { stored, .. } = value {
            let _ = stored;
        }
    });
    // W0449: lash::persistence::ProcessCompletionOutcome::from_stored [function]
    let _ = lash::persistence::ProcessCompletionOutcome::from_stored;
    // W0450: lash::plugins::ProcessEngine [trait]
    fn trait_witness_0450<T: lash::plugins::ProcessEngine>() {}
    // W0452: lash::plugins::ProcessEngine::kind [function]
    fn meth_0452<T: lash::plugins::ProcessEngine>(_: &T) {
        let _ = T::kind;
    }
    // W0455: lash::plugins::ProcessEngineContributionContext [struct]
    type_witness::<lash::plugins::ProcessEngineContributionContext>();
    // W0456: lash::plugins::ProcessEngineContributionContext::extensions [function]
    let _ = lash::plugins::ProcessEngineContributionContext::extensions;
    // W0457: lash::plugins::ProcessEngineContributionContext::new [function]
    let _ = lash::plugins::ProcessEngineContributionContext::new;
    // W0458: lash::plugins::ProcessEngineContributionContext::process_lifecycle_available [function]
    let _ = lash::plugins::ProcessEngineContributionContext::process_lifecycle_available;
    // W0459: lash::plugins::ProcessEngineContributionContext::trace_runtime [function]
    let _ = lash::plugins::ProcessEngineContributionContext::trace_runtime;
    // W0498: lash::process::ProcessId [type_alias]
    type_witness::<lash_vocabulary::identity::ProcessId>();
    // W0499: lash::plugins::ProcessInfraError [struct]
    type_witness::<lash::plugins::ProcessInfraError>();
    // W0500: lash::plugins::ProcessInfraError::new [function]
    let _ = lash::plugins::ProcessInfraError::new;
    // W0501: lash::process::ProcessOutcome [type_alias]
    type_witness::<lash::process::ProcessOutcome>();
    // W0502: lash::durability::ProcessOutcomeObserver [type_alias]
    type_witness::<lash::durability::ProcessOutcomeObserver>();
    // W0503: lash::plugins::ProcessRunOutcome [enum]
    type_witness::<lash::plugins::ProcessRunOutcome>();
    // W0506: lash::plugins::ProcessRunOutcome::Terminal [variant]
    variant_witness(|value: &lash::plugins::ProcessRunOutcome| {
        matches!(value, lash::plugins::ProcessRunOutcome::Terminal { .. })
    });
    // W0508: lash::plugins::ProcessRunOutcome::Terminal::output [field]
    field_witness(|value: &lash::plugins::ProcessRunOutcome| {
        let lash::plugins::ProcessRunOutcome::Terminal { output, .. } = value;
        let _ = output;
    });
    // W0527: lash::process::ProcessTombstone [struct]
    type_witness::<lash::process::ProcessTombstone>();
    // W0528: lash::process::ProcessTombstone::process_id [field]
    field_witness(|value: &lash::process::ProcessTombstone| {
        let _ = &value.process_id;
    });
    // W0529: lash::process::ProcessTombstone::pruned_at_ms [field]
    field_witness(|value: &lash::process::ProcessTombstone| {
        let _ = &value.pruned_at_ms;
    });
    // W0530: lash::process::ProcessTombstone::pruned_change_seq [field]
    field_witness(|value: &lash::process::ProcessTombstone| {
        let _ = &value.pruned_change_seq;
    });
    // W0531: lash::process::ProcessTombstone::terminal_label [field]
    field_witness(|value: &lash::process::ProcessTombstone| {
        let _ = &value.terminal_label;
    });
    // W0532: lash::plugins::ProtocolBeforeLlmCallContext::bindings [field]
    field_witness(|value: &lash::plugins::ProtocolBeforeLlmCallContext| {
        let _ = &value.bindings;
    });
    // W0536: lash::plugins::RuntimeExecutionContext::process_handle_json [function]
    let _ = lash::plugins::RuntimeExecutionContext::process_handle_json;
    // W0539: lash::plugins::RuntimeExecutionContext::start_child_process [function]
    let _ = lash::plugins::RuntimeExecutionContext::start_child_process(
        todo!(),
        todo!(),
        String::new(),
        todo!(),
    );
    // W0606: lash_execution_contracts::process::registry_transitions::RETIRED_PROCESS_STATUS_LABELS [constant]
    let _ = lash_execution_contracts::process::registry_transitions::RETIRED_PROCESS_STATUS_LABELS;
    // W0609: lash::testing::RuntimeNamedPhase [struct]
    type_witness::<lash::testing::RuntimeNamedPhase>();
    // W0610: lash::testing::RuntimeNamedPhase::begin [function]
    let _ = lash::testing::RuntimeNamedPhase::begin;
    // W0611: lash::testing::RuntimeTurnPhaseProbeSlot [struct]
    type_witness::<lash::testing::RuntimeTurnPhaseProbeSlot>();
    // W0612: lash::testing::RuntimeTurnPhaseProbeSlot::get_for_scope [function]
    let _ = lash::testing::RuntimeTurnPhaseProbeSlot::get_for_scope;
    // W0613: lash::testing::RuntimeTurnPhaseProbeSlot::set_for_scope [function]
    let _ = lash::testing::RuntimeTurnPhaseProbeSlot::set_for_scope;
    // W0614: lash::testing::RuntimeTurnPhaseProbeSlot::set_for_session [function]
    let _ = lash::testing::RuntimeTurnPhaseProbeSlot::set_for_session(
        todo!(),
        lash_kernel_value::identity::SessionId::from("x"),
        todo!(),
    );
    // W0623: lash::plugins::ProcessEngineRegistry [struct]
    type_witness::<lash::plugins::ProcessEngineRegistry>();
    // W0624: lash::plugins::ProcessEngineRegistry::new [function]
    let _ = lash::plugins::ProcessEngineRegistry::new;
    // W0625: lash::plugins::ProcessEngineRegistry::require [function]
    let _ = lash::plugins::ProcessEngineRegistry::require;
    // W0640: lash::persistence::QueuedCheckpointTurnInput [struct]
    type_witness::<lash::persistence::QueuedCheckpointTurnInput>();
    // W0641: lash::persistence::QueuedCheckpointTurnInput::messages [field]
    field_witness(|value: &lash::persistence::QueuedCheckpointTurnInput| {
        let _ = &value.messages;
    });
    // W0655: lash::plugins::PluginError::ProcessRegistryCursorBackendMismatch [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::ProcessRegistryCursorBackendMismatch { .. }
        )
    });
    // W0656: lash::plugins::PluginError::ProcessRegistryCursorBackendMismatch::actual [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessRegistryCursorBackendMismatch { actual, .. } =
            value
        {
            let _ = actual;
        }
    });
    // W0657: lash::plugins::PluginError::ProcessRegistryCursorBackendMismatch::expected [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessRegistryCursorBackendMismatch {
            expected, ..
        } = value
        {
            let _ = expected;
        }
    });
    // W0660: lash::process::ProcessService::list_visible_for_attempt [function]
    fn meth_0660<T: lash::process::ProcessService>(_: &T) {
        let _ = T::list_visible_for_attempt;
    }
    // W0676: lash::runtime::RuntimeEffectKind::ToolParentEnd [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectKind| {
        matches!(value, lash::runtime::RuntimeEffectKind::ToolParentEnd)
    });
    // W0686: lash::plugins::ProcessRunOutcome::is_terminal [function]
    let _ = lash::plugins::ProcessRunOutcome::is_terminal;
    // W0687: lash::plugins::ProcessRunOutcome::terminal_output [function]
    let _ = lash::plugins::ProcessRunOutcome::terminal_output;
    // W0692: lash::durability::ProcessLocalExecution::process_env_store [field]
    field_witness(|value: &lash::durability::ProcessLocalExecution| {
        let _ = &value.process_env_store;
    });
    // W0693: lash::tools::ToolIntentSubmissionRecord [struct]
    type_witness::<lash::tools::ToolIntentSubmissionRecord>();
    // W0694: lash::tools::ToolIntentSubmissionRecord::identity [field]
    field_witness(|value: &lash::tools::ToolIntentSubmissionRecord| {
        let _ = &value.identity;
    });
    // W0695: lash::tools::ToolIntentSubmissionRecord::kind [function]
    field_witness(|value: &lash::tools::ToolIntentSubmissionRecord| {
        let _ = value.kind();
    });
    // W0696: lash::tools::ToolIntentSubmissionRecord::payload_hash [field]
    field_witness(|value: &lash::tools::ToolIntentSubmissionRecord| {
        let _ = &value.payload_hash;
    });
    // W0697: lash::tools::ToolIntentSubmissionRecord::intent [field]
    field_witness(|value: &lash::tools::ToolIntentSubmissionRecord| {
        let _ = &value.intent;
    });
    // W0698: lash::tools::ToolIntentSubmissionRecord::settlement [field]
    field_witness(|value: &lash::tools::ToolIntentSubmissionRecord| {
        let _ = &value.settlement;
    });
    // W0700: lash::tools::ToolIntentSubmissionRecord::new [function]
    let _ = lash::tools::ToolIntentSubmissionRecord::new;
    // W0701: lash::tools::ToolIntentSubmissionAdmission [enum]
    type_witness::<lash::tools::ToolIntentSubmissionAdmission>();
    // W0702: lash::tools::ToolIntentSubmissionAdmission::Admitted [variant]
    variant_witness(|value: &lash::tools::ToolIntentSubmissionAdmission| {
        matches!(value, lash::tools::ToolIntentSubmissionAdmission::Admitted)
    });
    // W0703: lash::tools::ToolIntentSubmissionAdmission::Existing [variant]
    variant_witness(|value: &lash::tools::ToolIntentSubmissionAdmission| {
        matches!(
            value,
            lash::tools::ToolIntentSubmissionAdmission::Existing(..)
        )
    });
    // W0704: lash::tools::ToolIntentSubmissionAdmission::Existing::0 [field]
    field_witness(|value: &lash::tools::ToolIntentSubmissionAdmission| {
        if let lash::tools::ToolIntentSubmissionAdmission::Existing(f0) = value {
            let _ = f0;
        }
    });
    // W0705: lash::persistence::ProcessRegistry::admit_tool_intent_submission [function]
    fn meth_0705<T: lash::persistence::ProcessRegistry>(_: &T) {
        let _ = T::admit_tool_intent_submission;
    }
    // W0706: lash::persistence::ProcessRegistry::complete_tool_intent_submission [function]
    fn meth_0706<T: lash::persistence::ProcessRegistry>(_: &T) {
        let _ = T::complete_tool_intent_submission;
    }
    // W0709: lash::persistence::ForkSessionRequest::pending_observer_intents [field]
    field_witness(|value: &lash::persistence::ForkSessionRequest| {
        let _ = &value.pending_observer_intents;
    });
    // W0710: lash::persistence::ForkSessionReceipt::observed_processes [field]
    field_witness(|value: &lash::persistence::ForkSessionReceipt| {
        let _ = &value.observed_processes;
    });
    // W0711: lash::persistence::SessionMeta::pending_observer_intents [field]
    field_witness(|value: &lash::persistence::SessionMeta| {
        let _ = &value.pending_observer_intents;
    });
    // W0712: lash::persistence::SessionStoreCreateRequest::pending_observer_intents [field]
    field_witness(|value: &lash::persistence::SessionStoreCreateRequest| {
        let _ = &value.pending_observer_intents;
    });
    // W0713: lash_core::facade_support::SessionObserverIntent [struct]
    type_witness::<lash_core::facade_support::SessionObserverIntent>();
    // W0716: lash_core::facade_support::SessionObserverIntent::host_requested [function]
    let _ = lash_core::facade_support::SessionObserverIntent::host_requested(
        lash_vocabulary::identity::ProcessId::fixture("x"),
    );
    // W0717: lash_core::facade_support::SessionObserverIntent::process_id [field]
    field_witness(|value: &lash_core::facade_support::SessionObserverIntent| {
        let _ = &value.process_id;
    });
    // W0724: lash::plugins::PluginError::ProcessUnknown [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(value, lash::plugins::PluginError::ProcessUnknown { .. })
    });
    // W0725: lash::plugins::PluginError::ProcessUnknown::process_id [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessUnknown { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // W0728: lash::process::ProcessStatus::is_retired [function]
    let _ = lash::process::ProcessStatus::is_retired;
    // W0732: lash::durability::ProcessLocalExecution::execute [function]
    let _ = lash::durability::ProcessLocalExecution::execute;
}
