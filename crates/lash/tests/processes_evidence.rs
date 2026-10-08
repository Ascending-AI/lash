//! Compile-time witnesses for process-area facade and integrator contracts.
//!
//! FIG-2106 drains the ledger's `processes`/`unused-justify` slice: at the
//! dispatch-time recount the slice held 733 rows. The 647 rows whose item
//! still exists are type-checked across this file and
//! `processes_evidence_b.rs` through the path a host or integrator would
//! name — `lash::` for facade surface, `lash_core::` for internal seams the
//! integrator classes consume directly. The 86 rows whose item no longer
//! exists anywhere in this workspace (typed `ProcessRef` migration,
//! parent-end plan removal, retired status filters, and other upstream
//! surface changes since the ledger retired) are listed in the pull request
//! rather than witnessed here.

#![cfg(feature = "testing")]
#![allow(dead_code, unreachable_code, unused_variables, unused_imports)]
#![allow(clippy::all)]

fn type_witness<T>() {}
fn member_witness<T>(_: T) {}
fn field_witness<T>(_: impl FnOnce(&T)) {}
fn variant_witness<T>(_: impl FnOnce(&T) -> bool) {}

fn processes_area_witnesses() {
    // W0007: lash::runtime::RuntimeEffectLocalExecutor::with_process_turn_cancellation [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::with_process_turn_cancellation;
    // W0008: lash::runtime::RuntimeEffectLocalExecutor::with_turn_cancel_observation [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::with_turn_cancel_observation;
    // W0011: lash::SessionCreateRequest::observed_processes [field]
    field_witness(|value: &lash::SessionCreateRequest| {
        let _ = &value.observed_processes;
    });
    // W0012: lash::SessionCreateRequest::with_observed_processes [function]
    let _: fn(lash::SessionCreateRequest, Vec<lash::ProcessId>) -> lash::SessionCreateRequest =
        lash::SessionCreateRequest::with_observed_processes;
    // W0013: lash::durability::DurableProcessWorker [struct]
    type_witness::<lash::durability::DurableProcessWorker>();
    // W0014: lash::durability::DurableProcessWorker::config [function]
    let _ = lash::durability::DurableProcessWorker::config;
    // W0018: lash::durability::DurableProcessWorker::new [function]
    let _ = lash::durability::DurableProcessWorker::new;
    // W0021: lash::durability::DurableProcessWorkerConfig [struct]
    type_witness::<lash::durability::DurableProcessWorkerConfig>();
    // W0022: lash::durability::DurableProcessWorkerConfig::from_plugin_factories [function]
    let _: fn(
        Vec<std::sync::Arc<dyn lash::plugins::PluginFactory>>,
        lash::durability::RuntimeHostConfig,
        lash::process::ProcessWorkWiring,
        lash::persistence::LeaseOwnerIdentity,
    ) -> lash::durability::DurableProcessWorkerConfig =
        lash::durability::DurableProcessWorkerConfig::from_plugin_factories;
    // W0023: lash::durability::DurableProcessWorkerConfig::from_plugin_stack [function]
    let _ = lash::durability::DurableProcessWorkerConfig::from_plugin_stack;
    // W0024: lash::durability::DurableProcessWorkerConfig::lease_owner [field]
    field_witness(|value: &lash::durability::DurableProcessWorkerConfig| {
        let _ = &value.lease_owner;
    });
    // W0025: lash::durability::DurableProcessWorkerConfig::new [function]
    let _ = lash::durability::DurableProcessWorkerConfig::new;
    // W0026: lash::durability::DurableProcessWorkerConfig::plugin_host [field]
    field_witness(|value: &lash::durability::DurableProcessWorkerConfig| {
        let _ = &value.plugin_host;
    });
    // W0032: lash::durability::DurableProcessWorkerConfig::runtime_host [field]
    field_witness(|value: &lash::durability::DurableProcessWorkerConfig| {
        let _ = &value.runtime_host;
    });
    // W0034: lash::durability::DurableProcessWorkerConfig::session_store_factory [function]
    let _ = lash::durability::DurableProcessWorkerConfig::session_store_factory;
    // W0079: lash::durability::RuntimeHostConfig::process_engines [field]
    field_witness(|value: &lash::durability::RuntimeHostConfig| {
        let _ = &value.process_engines;
    });
    // W0081: lash::durability::RuntimeHostConfig::with_process_env_store [function]
    let _ = lash::durability::RuntimeHostConfig::with_process_env_store;
    // W0082: lash::durability::RuntimeHostConfig::with_process_tool_visibility_filter [function]
    let _ = lash::durability::RuntimeHostConfig::with_process_tool_visibility_filter;
    // W0090: lash::observe::SessionObservationEventPayload::ProcessChanged::kind [field]
    field_witness(|value: &lash::observe::SessionObservationEventPayload| {
        if let lash::observe::SessionObservationEventPayload::ProcessChanged { kind, .. } = value {
            let _ = kind;
        }
    });
    // W0091: lash::observe::SessionProcessEventKind [enum]
    type_witness::<lash::observe::SessionProcessEventKind>();
    // W0092: lash::observe::SessionProcessEventKind::Cancelled [variant]
    variant_witness(|value: &lash::observe::SessionProcessEventKind| {
        matches!(
            value,
            lash::observe::SessionProcessEventKind::Cancelled { .. }
        )
    });
    // W0093: lash::observe::SessionProcessEventKind::Started [variant]
    variant_witness(|value: &lash::observe::SessionProcessEventKind| {
        matches!(
            value,
            lash::observe::SessionProcessEventKind::Started { .. }
        )
    });
    // W0094: lash::persistence::LeaseOwnerIdentity::engine_process_execution [function]
    let _ = lash::persistence::LeaseOwnerIdentity::engine_process_execution(todo!(), "exec");
    // W0095: lash::persistence::LeaseOwnerIdentity::engine_process_execution_id [function]
    let _ = lash::persistence::LeaseOwnerIdentity::engine_process_execution_id;
    // W0096: lash::persistence::ProcessExecutionEnvStore [trait]
    fn trait_witness_0096<T: lash::persistence::ProcessExecutionEnvStore>() {}
    // W0097: lash::persistence::ProcessExecutionEnvStore::get_process_execution_env [function]
    fn meth_0097<T: lash::persistence::ProcessExecutionEnvStore>(_: &T) {
        let _ = T::get_process_execution_env;
    }
    // W0111: lash::plugins::PluginError::ProcessAlreadyTerminal [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::ProcessAlreadyTerminal { .. }
        )
    });
    // W0112: lash::plugins::PluginError::ProcessAlreadyTerminal::process_id [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessAlreadyTerminal { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // W0113: lash::plugins::PluginError::ProcessAlreadyTerminal::status [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessAlreadyTerminal { status, .. } = value {
            let _ = status;
        }
    });
    // W0120: lash::plugins::PluginError::ProcessExecutionSuperseded [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::ProcessExecutionSuperseded { .. }
        )
    });
    // W0121: lash::plugins::PluginError::ProcessExecutionSuperseded::process_id [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessExecutionSuperseded { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // W0122: lash::plugins::PluginError::ProcessNoLongerRetained [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::ProcessNoLongerRetained { .. }
        )
    });
    // W0123: lash::plugins::PluginError::ProcessNoLongerRetained::pruned_at_ms [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessNoLongerRetained { pruned_at_ms, .. } = value {
            let _ = pruned_at_ms;
        }
    });
    // W0124: lash::plugins::PluginError::ProcessNoLongerRetained::terminal_label [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessNoLongerRetained { terminal_label, .. } = value {
            let _ = terminal_label;
        }
    });
    // W0125: lash::plugins::PluginError::ProcessNotVisible [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(value, lash::plugins::PluginError::ProcessNotVisible { .. })
    });
    // W0126: lash::plugins::PluginError::ProcessNotVisible::process_id [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessNotVisible { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // W0127: lash::plugins::PluginError::ProcessTerminalOutcomeMismatch [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::ProcessTerminalOutcomeMismatch { .. }
        )
    });
    // W0128: lash::plugins::PluginError::ProcessTerminalOutcomeMismatch::declared_status [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessTerminalOutcomeMismatch {
            declared_status, ..
        } = value
        {
            let _ = declared_status;
        }
    });
    // W0129: lash::plugins::PluginError::ProcessTerminalOutcomeMismatch::outcome_status [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessTerminalOutcomeMismatch {
            outcome_status, ..
        } = value
        {
            let _ = outcome_status;
        }
    });
    // W0133: lash::plugins::PluginError::UnknownProcessEventKind [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::UnknownProcessEventKind { .. }
        )
    });
    // W0135: lash::plugins::PluginHost::install_process_engine_contributions [function]
    let _ = lash::plugins::PluginHost::install_process_engine_contributions;
    // W0136: lash::process::AbandonEvidence [struct]
    type_witness::<lash::process::AbandonEvidence>();
    // W0138: lash::process::AbandonWriter [enum]
    type_witness::<lash::process::AbandonWriter>();
    // W0139: lash::process::CausalRef::ProcessEvent [variant]
    variant_witness(|value: &lash::process::CausalRef| {
        matches!(value, lash::process::CausalRef::ProcessEvent { .. })
    });
    // W0140: lash::process::CausalRef::ProcessEvent::process_id [field]
    field_witness(|value: &lash::process::CausalRef| {
        if let lash::process::CausalRef::ProcessEvent { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // W0141: lash::process::CausalRef::ProcessEvent::sequence [field]
    field_witness(|value: &lash::process::CausalRef| {
        if let lash::process::CausalRef::ProcessEvent { sequence, .. } = value {
            let _ = sequence;
        }
    });
    // W0142: lash::process::CausalRef::ToolCall [variant]
    variant_witness(|value: &lash::process::CausalRef| {
        matches!(value, lash::process::CausalRef::ToolCall { .. })
    });
    // W0143: lash::process::CausalRef::ToolCall::call_id [field]
    field_witness(|value: &lash::process::CausalRef| {
        if let lash::process::CausalRef::ToolCall { call_id, .. } = value {
            let _ = call_id;
        }
    });
    // W0144: lash::process::CausalRef::ToolCall::session_id [field]
    field_witness(|value: &lash::process::CausalRef| {
        if let lash::process::CausalRef::ToolCall { session_id, .. } = value {
            let _ = session_id;
        }
    });
    // W0145: lash::process::ProcessAwaitOutput::Abandoned [variant]
    variant_witness(|value: &lash::process::ProcessAwaitOutput| {
        matches!(value, lash::process::ProcessAwaitOutput::Abandoned { .. })
    });
    // W0146: lash::process::ProcessAwaitOutput::Abandoned::control [field]
    field_witness(|value: &lash::process::ProcessAwaitOutput| {
        if let lash::process::ProcessAwaitOutput::Abandoned { control, .. } = value {
            let _ = control;
        }
    });
    // W0147: lash::process::ProcessAwaitOutput::Abandoned::evidence [field]
    field_witness(|value: &lash::process::ProcessAwaitOutput| {
        if let lash::process::ProcessAwaitOutput::Abandoned { evidence, .. } = value {
            let _ = evidence;
        }
    });
    // W0148: lash::process::ProcessAwaitOutput::NoLongerRetained [variant]
    variant_witness(|value: &lash::process::ProcessAwaitOutput| {
        matches!(
            value,
            lash::process::ProcessAwaitOutput::NoLongerRetained { .. }
        )
    });
    // W0149: lash::process::ProcessAwaitOutput::NoLongerRetained::pruned_at_ms [field]
    field_witness(|value: &lash::process::ProcessAwaitOutput| {
        if let lash::process::ProcessAwaitOutput::NoLongerRetained { pruned_at_ms, .. } = value {
            let _ = pruned_at_ms;
        }
    });
    // W0150: lash::process::ProcessAwaitOutput::NoLongerRetained::terminal_label [field]
    field_witness(|value: &lash::process::ProcessAwaitOutput| {
        if let lash::process::ProcessAwaitOutput::NoLongerRetained { terminal_label, .. } = value {
            let _ = terminal_label;
        }
    });
    // W0153: lash::process::ProcessChangeHub [struct]
    type_witness::<lash::process::ProcessChangeHub>();
    // W0154: lash::process::ProcessChangeHub::new [function]
    let _ = lash::process::ProcessChangeHub::new;
    // W0155: lash::process::ProcessChangeHub::notify [function]
    let _ = lash::process::ProcessChangeHub::notify;
    // W0156: lash::process::ProcessChangeHub::subscribe [function]
    let _ = lash::process::ProcessChangeHub::subscribe;
    // W0159: lash::process::ProcessCompletionAuthority::WorkflowKey [variant]
    variant_witness(|value: &lash::process::ProcessCompletionAuthority| {
        matches!(
            value,
            lash::process::ProcessCompletionAuthority::WorkflowKey { .. }
        )
    });
    // W0160: lash::process::ProcessCompletionAuthority::WorkflowKey::workflow_key [field]
    field_witness(|value: &lash::process::ProcessCompletionAuthority| {
        if let lash::process::ProcessCompletionAuthority::WorkflowKey { workflow_key, .. } = value {
            let _ = workflow_key;
        }
    });
    // W0161: lash::process::ProcessCompletionAuthority::validate [function]
    let _ = lash::process::ProcessCompletionAuthority::validate;
    // W0167: lash::process::ProcessEvent [struct]
    type_witness::<lash::process::ProcessEvent>();
    // W0169: lash::process::ProcessEvent::invocation [field]
    field_witness(|value: &lash::process::ProcessEvent| {
        let _ = &value.invocation;
    });
    // W0170: lash::process::ProcessEvent::occurred_at [field]
    field_witness(|value: &lash::process::ProcessEvent| {
        let _ = &value.occurred_at;
    });
    // W0172: lash::process::ProcessEvent::process_id [field]
    field_witness(|value: &lash::process::ProcessEvent| {
        let _ = &value.process_id;
    });
    // W0174: lash::process::ProcessEvent::sequence [field]
    field_witness(|value: &lash::process::ProcessEvent| {
        let _ = &value.sequence;
    });
    // W0176: lash::process::ProcessEventAppendRequest::cancel_requested [function]
    let _ = lash::process::ProcessEventAppendRequest::cancel_requested;
    // W0178: lash::process::ProcessEventAppendRequest::external_ref_set [function]
    let _ = lash::process::ProcessEventAppendRequest::external_ref_set;
    // W0179: lash::process::ProcessEventAppendRequest::first_started [function]
    let _ = lash::process::ProcessEventAppendRequest::first_started;
    // W0180: lash::process::ProcessEventAppendRequest::observer_added [function]
    let _ = lash::process::ProcessEventAppendRequest::observer_added;
    // W0181: lash::process::ProcessEventAppendRequest::observer_removed [function]
    let _ = lash::process::ProcessEventAppendRequest::observer_removed;
    // W0183: lash::process::ProcessEventAppendRequest::replay [field]
    field_witness(|value: &lash::process::ProcessEventAppendRequest| {
        let _ = &value.replay;
    });
    // W0184: lash::process::ProcessEventAppendRequest::wait_cleared [function]
    let _ = lash::process::ProcessEventAppendRequest::wait_cleared;
    // W0185: lash::process::ProcessEventAppendRequest::wait_entered [function]
    let _ = lash::process::ProcessEventAppendRequest::wait_entered;
    // W0186: lash::process::ProcessEventAppendReceipt [struct]
    type_witness::<lash::process::ProcessEventAppendReceipt>();
    // W0187: lash::process::ProcessEventAppendReceipt::event [field]
    field_witness(|value: &lash::process::ProcessEventAppendReceipt| {
        let _ = &value.event;
    });
    // W0190: lash::process::ProcessExecutionContext [struct]
    type_witness::<lash::process::ProcessExecutionContext>();
    // W0191: lash::process::ProcessExecutionContext::causal_invocation [field]
    field_witness(|value: &lash::process::ProcessExecutionContext| {
        let _ = &value.causal_invocation;
    });
    // W0192: lash::process::ProcessExecutionContext::execution_write_authority [field]
    field_witness(|value: &lash::process::ProcessExecutionContext| {
        let _ = &value.execution_write_authority;
    });
    // W0193: lash::process::ProcessExecutionContext::is_empty [function]
    let _ = lash::process::ProcessExecutionContext::is_empty;
    // W0194: lash::process::ProcessExecutionContext::with_causal_invocation [function]
    let _ = lash::process::ProcessExecutionContext::with_causal_invocation;
    // W0195: lash::process::ProcessExecutionContext::with_execution_write_authority [function]
    let _ = lash::process::ProcessExecutionContext::with_execution_write_authority;
    // W0196: lash::process::ProcessExecutionEnvSpec::from_store_bytes [function]
    let _ = lash::process::ProcessExecutionEnvSpec::from_store_bytes;
    // W0197: lash::process::ProcessExecutionEnvSpec::plugin_config [field]
    field_witness(|value: &lash::process::ProcessExecutionEnvSpec| {
        let _ = &value.plugin_config;
    });
    // W0198: lash::process::ProcessExecutionEnvSpec::policy [field]
    field_witness(|value: &lash::process::ProcessExecutionEnvSpec| {
        let _ = &value.policy;
    });
    // W0199: lash::process::ProcessExecutionEnvSpec::to_store_bytes [function]
    let _ = lash::process::ProcessExecutionEnvSpec::to_store_bytes;
    // W0200: lash::process::ProcessHandleView::new [function]
    let _ = lash::process::ProcessHandleView::new(todo!(), todo!(), todo!());
    // W0201: lash::process::ProcessIdentity::definition [field]
    field_witness(|value: &lash::process::ProcessIdentity| {
        let _ = &value.definition;
    });
    // W0202: lash::process::ProcessIdentity::kind [field]
    field_witness(|value: &lash::process::ProcessIdentity| {
        let _ = &value.kind;
    });
    // W0203: lash::process::ProcessIdentity::label [field]
    field_witness(|value: &lash::process::ProcessIdentity| {
        let _ = &value.label;
    });
    // W0204: lash::process::ProcessInput::SessionTurn [variant]
    variant_witness(|value: &lash::process::ProcessInput| {
        matches!(value, lash::process::ProcessInput::SessionTurn { .. })
    });
    // W0205: lash::process::ProcessInput::SessionTurn::create_request [field]
    field_witness(|value: &lash::process::ProcessInput| {
        if let lash::process::ProcessInput::SessionTurn { create_request, .. } = value {
            let _ = create_request;
        }
    });
    // W0206: lash::process::ProcessInput::SessionTurn::definition_key [field]
    field_witness(|value: &lash::process::ProcessInput| {
        if let lash::process::ProcessInput::SessionTurn { definition_key, .. } = value {
            let _ = definition_key;
        }
    });
    // W0207: lash::process::ProcessInput::SessionTurn::result [field]
    field_witness(|value: &lash::process::ProcessInput| {
        if let lash::process::ProcessInput::SessionTurn { result, .. } = value {
            let _ = result;
        }
    });
    // W0208: lash::process::ProcessInput::SessionTurn::turn_input [field]
    field_witness(|value: &lash::process::ProcessInput| {
        if let lash::process::ProcessInput::SessionTurn { turn_input, .. } = value {
            let _ = turn_input;
        }
    });
    // W0217: lash::process::ProcessListFilter::created_at_end_ms [field]
    field_witness(|value: &lash::process::ProcessListFilter| {
        let _ = &value.created_at_end_ms;
    });
    // W0218: lash::process::ProcessListFilter::created_at_start_ms [field]
    field_witness(|value: &lash::process::ProcessListFilter| {
        let _ = &value.created_at_start_ms;
    });
    // W0219: lash::process::ProcessListFilter::identity_kind [field]
    field_witness(|value: &lash::process::ProcessListFilter| {
        let _ = &value.identity_kind;
    });
    // W0220: lash::process::ProcessListFilter::identity_label [field]
    field_witness(|value: &lash::process::ProcessListFilter| {
        let _ = &value.identity_label;
    });
    // W0221: lash::process::ProcessLiveReferenceView::from_records [function]
    let _ = lash::process::ProcessLiveReferenceView::from_records(std::iter::empty::<
        &'static lash::process::ProcessRecord,
    >());
    // W0222: lash::process::ProcessObserverBy::Host [variant]
    variant_witness(|value: &lash::process::ProcessObserverBy| {
        matches!(value, lash::process::ProcessObserverBy::Host { .. })
    });
    // W0223: lash::process::ProcessObserverBy::Host::operation_id [field]
    field_witness(|value: &lash::process::ProcessObserverBy| {
        let lash::process::ProcessObserverBy::Host { operation_id, .. } = value;
        let _ = operation_id;
    });
    // W0224: lash::process::ProcessOpScope [struct]
    type_witness::<lash::process::ProcessOpScope>();
    // W0225: lash::process::ProcessOpScope::agent_frame_id [function]
    let _ = lash::process::ProcessOpScope::agent_frame_id;
    // W0226: lash::process::ProcessOpScope::new [function]
    let _ = lash::process::ProcessOpScope::new(todo!());
    // W0227: lash::process::ProcessOpScope::with_agent_frame_id [function]
    let _ = lash::process::ProcessOpScope::with_agent_frame_id;
    // W0228: lash::process::ProcessOpScope::with_parent_invocation [function]
    let _ = lash::process::ProcessOpScope::with_parent_invocation;
    // W0229: lash::process::ProcessOriginator::Host [variant]
    variant_witness(|value: &lash::process::ProcessOriginator| {
        matches!(value, lash::process::ProcessOriginator::Host { .. })
    });
    // W0230: lash::process::ProcessOriginator::Host::scope [field]
    field_witness(|value: &lash::process::ProcessOriginator| {
        if let lash::process::ProcessOriginator::Host { scope, .. } = value {
            let _ = scope;
        }
    });
    // W0231: lash::process::ProcessOriginator::host [function]
    let _ = lash::process::ProcessOriginator::host();
    // W0232: lash::process::ProcessRecord::from_prepared_registration [function]
    let _ = lash::process::ProcessRecord::from_prepared_registration;
    // W0233: lash::process::ProcessRecord::from_registration [function]
    let _ = lash::process::ProcessRecord::from_registration;
    // W0234: lash::process::ProcessRecord::from_registration_with_clock [function]
    let _ = lash::process::ProcessRecord::from_registration_with_clock;
    // W0236: lash::process::ProcessRegistry::add_observer [function]
    fn meth_0236<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::add_observer;
    }
    // W0237: lash::process::ProcessRegistry::append_event_with_authority [function]
    fn meth_0237<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::append_event_with_authority;
    }
    // W0239: lash::process::ProcessRegistry::clear_process_wait_with_authority [function]
    fn meth_0239<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::clear_process_wait_with_authority;
    }
    // W0242: lash::process::ProcessRegistry::delete_session_process_state [function]
    fn meth_0242<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::delete_session_process_state;
    }
    // W0245: lash::process::ProcessRegistry::get_process [function]
    fn meth_0245<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::get_process;
    }
    // W0246: lash::process::ProcessRegistry::list_live_observed_by [function]
    fn meth_0246<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::list_live_observed_by;
    }
    // W0247: lash::process::ProcessRegistry::list_observed_by [function]
    fn meth_0247<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::list_observed_by;
    }
    // W0250: lash::process::ProcessRegistry::processes_changed_since [function]
    fn meth_0250<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::processes_changed_since;
    }
    // W0252: lash::process::ProcessRegistry::record_first_started_with_authority [function]
    fn meth_0252<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::record_first_started_with_authority;
    }
    // W0255: lash::process::ProcessRegistry::set_process_wait_with_authority [function]
    fn meth_0255<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::set_process_wait_with_authority;
    }
    // W0258: lash::process::ProcessRegistry::with_runtime_clock [function]
    fn meth_0258<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::with_runtime_clock;
    }
    // W0259: lash::process::ProcessService [trait]
    fn trait_witness_0259<T: lash::process::ProcessService>() {}
    // W0260: lash::process::ProcessService::await_process [function]
    fn meth_0260<T: lash::process::ProcessService>(_: &T) {
        let _ = T::await_process;
    }
    // W0261: lash::process::ProcessService::cancel [function]
    fn meth_0261<T: lash::process::ProcessService>(_: &T) {
        let _ = T::cancel;
    }
    // W0262: lash::process::ProcessService::cancel_recorded_intent [function]
    fn meth_0262<T: lash::process::ProcessService>(_: &T) {
        let _ = T::cancel_recorded_intent;
    }
    // W0263: lash::process::ProcessService::cancel_all_visible [function]
    fn meth_0263<T: lash::process::ProcessService>(_: &T) {
        let _ = T::cancel_all_visible;
    }
    // W0265: lash::process::ProcessService::list_visible [function]
    fn meth_0265<T: lash::process::ProcessService>(_: &T) {
        let _ = T::list_visible;
    }
    // W0269: lash::process::ProcessService::start [function]
    fn meth_0269<T: lash::process::ProcessService>(_: &T) {
        let _ = T::start;
    }
    // W0270: lash::process::ProcessService::stage_recorded_start [function]
    fn meth_0270<T: lash::process::ProcessService>(_: &T) {
        let _ = T::stage_recorded_start;
    }
    // W0271: lash::process::ProcessService::start_from_request [function]
    fn meth_0271<T: lash::process::ProcessService>(_: &T) {
        let _ = T::start_from_request;
    }
    // W0272: lash::process::ProcessService::transfer [function]
    fn meth_0272<T: lash::process::ProcessService>(_: &T) {
        let _ = T::transfer;
    }
    // W0273: lash::process::ProcessService::validate_visible [function]
    fn meth_0273<T: lash::process::ProcessService>(_: &T) {
        let _ = T::validate_visible;
    }
    // W0274: lash::process::ProcessSessionDeleteReport [struct]
    type_witness::<lash::process::ProcessSessionDeleteReport>();
    // W0276: lash::process::ProcessSessionDeleteReport::removed_observer_count [field]
    field_witness(|value: &lash::process::ProcessSessionDeleteReport| {
        let _ = &value.removed_observer_count;
    });
    // W0277: lash::process::ProcessSessionDeleteReport::session_id [field]
    field_witness(|value: &lash::process::ProcessSessionDeleteReport| {
        let _ = &value.session_id;
    });
    // W0278: lash::process::ProcessStartOptions [struct]
    type_witness::<lash::process::ProcessStartOptions>();
    // W0279: lash::process::ProcessStartOptions::execution_context [function]
    let _ = lash::process::ProcessStartOptions::execution_context;
    // W0280: lash::process::ProcessStartOptions::initial_observers [field]
    field_witness(|value: &lash::process::ProcessStartOptions| {
        let _ = &value.initial_observers;
    });
    // W0281: lash::process::ProcessStartOptions::new [function]
    let _ = lash::process::ProcessStartOptions::new();
    // W0282: lash::process::ProcessStartOptions::spawn_provenance [field]
    field_witness(|value: &lash::process::ProcessStartOptions| {
        let _ = &value.spawn_provenance;
    });
    // W0283: lash::process::ProcessStartOptions::with_initial_observer [function]
    let _ = lash::process::ProcessStartOptions::with_initial_observer(
        todo!(),
        lash::SessionId::from("x"),
    );
    // W0284: lash::process::ProcessStartOptions::with_initial_observers [function]
    let _ = lash::process::ProcessStartOptions::with_initial_observers(
        todo!(),
        std::iter::empty::<lash::SessionId>(),
    );
    // W0285: lash::process::ProcessStartOptions::with_spawn_provenance [function]
    let _ = lash::process::ProcessStartOptions::with_spawn_provenance;
    // W0286: lash::process::ProcessStartRequest [struct]
    type_witness::<lash::process::ProcessStartRequest>();
    // W0288: lash::process::ProcessStartRequest::env_ref [field]
    field_witness(|value: &lash::process::ProcessStartRequest| {
        let _ = &value.env_ref;
    });
    // W0292: lash::process::ProcessStartRequest::identity [field]
    field_witness(|value: &lash::process::ProcessStartRequest| {
        let _ = &value.identity;
    });
    // W0293: lash::process::ProcessStartRequest::input [field]
    field_witness(|value: &lash::process::ProcessStartRequest| {
        let _ = &value.input;
    });
    // W0294: lash::process::ProcessStartRequest::into_registration [function]
    let _ = lash::process::ProcessStartRequest::into_registration;
    // W0296: lash::process::ProcessStartRequest::new [function]
    let _ = lash::process::ProcessStartRequest::new(
        todo!() as lash::process::ProcessStartTarget,
        todo!(),
        lash::process::LifetimeDecision::Detached,
    );
    // W0297: lash::process::ProcessStartRequest::observers [field]
    field_witness(|value: &lash::process::ProcessStartRequest| {
        let _ = &value.observers;
    });
    // W0298: lash::process::ProcessStartRequest::originator [field]
    field_witness(|value: &lash::process::ProcessStartRequest| {
        let _ = &value.originator;
    });
    // W0300: lash::process::ProcessStartRequest::with_env_ref [function]
    let _ = lash::process::ProcessStartRequest::with_env_ref;
    // W0305: lash::process::ProcessStartRequest::with_observers [function]
    let _ = lash::process::ProcessStartRequest::with_observers(
        todo!(),
        std::iter::empty::<lash::SessionId>(),
    );
    // W0307: lash::process::ProcessStatus::Abandoned [variant]
    variant_witness(|value: &lash::process::ProcessStatus| {
        matches!(value, lash::process::ProcessStatus::Abandoned)
    });
    // W0308: lash::process::ProcessStatus::Waiting [variant]
    variant_witness(|value: &lash::process::ProcessStatus| {
        matches!(value, lash::process::ProcessStatus::Waiting)
    });
}
