//! Compile-time witnesses for process-area facade and integrator contracts.
//!
//! FIG-2106 drains the ledger's `processes`/`unused-justify` slice: at the
//! dispatch-time recount the slice held 733 rows. The 647 rows whose item
//! still exists are type-checked across this file and
//! `processes_evidence_b.rs` through the path a host or integrator would
//! name — `lash::` for facade surface, `lash_core::` for internal seams the
//! integrator classes consume directly. The 86 rows whose item no longer
//! exists anywhere in this workspace (typed process-reference migration,
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
    let _ = lash::persistence::LeaseOwnerIdentity::engine_process_execution(
        todo!(),
        lash::persistence::LeaseIncarnationId::new("exec"),
    );
    // W0095: lash::persistence::LeaseOwnerIdentity::engine_process_execution_id [function]
    let _ = lash::persistence::LeaseOwnerIdentity::engine_process_execution_id;
    // W0096: lash::persistence::ProcessEnvStore [trait]
    fn trait_witness_0096<T: lash::persistence::ProcessEnvStore>() {}
    // W0097: lash::persistence::ProcessEnvStore::get_process_env [function]
    fn meth_0097<T: lash::persistence::ProcessEnvStore>(_: &T) {
        let _ = T::get_process_env;
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
    // W0159: lash::persistence::ProcessCompletionAuthority::WorkflowKey [variant]
    variant_witness(|value: &lash::persistence::ProcessCompletionAuthority| {
        matches!(
            value,
            lash::persistence::ProcessCompletionAuthority::WorkflowKey { .. }
        )
    });
    // W0160: lash::persistence::ProcessCompletionAuthority::WorkflowKey::workflow_key [field]
    field_witness(|value: &lash::persistence::ProcessCompletionAuthority| {
        if let lash::persistence::ProcessCompletionAuthority::WorkflowKey { workflow_key, .. } =
            value
        {
            let _ = workflow_key;
        }
    });
    // W0161: lash::persistence::ProcessCompletionAuthority::validate [function]
    let _ = lash::persistence::ProcessCompletionAuthority::validate;
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
    // W0176: lash::persistence::ProcessEventAppendRequest::cancel_requested [function]
    let _ = lash::persistence::ProcessEventAppendRequest::cancel_requested;
    // W0178: lash::persistence::ProcessEventAppendRequest::external_ref_set [function]
    let _ = lash::persistence::ProcessEventAppendRequest::external_ref_set;
    // W0179: lash::persistence::ProcessEventAppendRequest::first_started [function]
    let _ = lash::persistence::ProcessEventAppendRequest::first_started;
    // W0180: lash::persistence::ProcessEventAppendRequest::observer_added [function]
    let _ = lash::persistence::ProcessEventAppendRequest::observer_added;
    // W0181: lash::persistence::ProcessEventAppendRequest::observer_removed [function]
    let _ = lash::persistence::ProcessEventAppendRequest::observer_removed;
    // W0183: lash::persistence::ProcessEventAppendRequest::replay [field]
    field_witness(|value: &lash::persistence::ProcessEventAppendRequest| {
        let _ = &value.replay;
    });
    // W0184: lash::persistence::ProcessEventAppendRequest::wait_cleared [function]
    let _ = lash::persistence::ProcessEventAppendRequest::wait_cleared;
    // W0185: lash::persistence::ProcessEventAppendRequest::wait_entered [function]
    let _ = lash::persistence::ProcessEventAppendRequest::wait_entered;
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
    // W0196: lash::process::ProcessEnv::from_store_bytes [function]
    let _ = lash::process::ProcessEnv::from_store_bytes;
    // W0197: lash::process::ProcessEnv::components [function]
    let _ = lash::process::ProcessEnv::components;
    // W0198: lash::process::ProcessEnv::tools [function]
    let _ = lash::process::ProcessEnv::tools;
    // W0199: lash::process::ProcessEnv::to_store_bytes [function]
    let _ = lash::process::ProcessEnv::to_store_bytes;
    // W0200: lash::persistence::ProcessHandleView::new [function]
    let _ = lash::persistence::ProcessHandleView::new(todo!(), todo!(), todo!());
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
    // W0217: lash::persistence::ProcessListFilter::created_at_end_ms [field]
    field_witness(|value: &lash::persistence::ProcessListFilter| {
        let _ = &value.created_at_end_ms;
    });
    // W0218: lash::persistence::ProcessListFilter::created_at_start_ms [field]
    field_witness(|value: &lash::persistence::ProcessListFilter| {
        let _ = &value.created_at_start_ms;
    });
    // W0219: lash::persistence::ProcessListFilter::identity_kind [field]
    field_witness(|value: &lash::persistence::ProcessListFilter| {
        let _ = &value.identity_kind;
    });
    // W0220: lash::persistence::ProcessListFilter::identity_label [field]
    field_witness(|value: &lash::persistence::ProcessListFilter| {
        let _ = &value.identity_label;
    });
    // W0221: lash::persistence::ProcessLiveReferenceView::from_records [function]
    let _ = lash::persistence::ProcessLiveReferenceView::from_records(std::iter::empty::<
        &'static lash::persistence::ProcessRecord,
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
    // W0224: lash::persistence::ProcessOpScope [struct]
    type_witness::<lash::persistence::ProcessOpScope>();
    // W0225: lash::persistence::ProcessOpScope::agent_frame_id [function]
    let _ = lash::persistence::ProcessOpScope::agent_frame_id;
    // W0226: lash::persistence::ProcessOpScope::new [function]
    let _ = lash::persistence::ProcessOpScope::new(todo!());
    // W0227: lash::persistence::ProcessOpScope::with_agent_frame_id [function]
    let _ = lash::persistence::ProcessOpScope::with_agent_frame_id;
    // W0228: lash::persistence::ProcessOpScope::with_parent_invocation [function]
    let _ = lash::persistence::ProcessOpScope::with_parent_invocation;
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
    // W0232: lash::persistence::ProcessRecord::from_prepared_registration [function]
    let _ = lash::persistence::ProcessRecord::from_prepared_registration;
    // W0233: lash::persistence::ProcessRecord::from_registration [function]
    let _ = lash::persistence::ProcessRecord::from_registration;
    // W0234: lash::persistence::ProcessRecord::from_registration_with_clock [function]
    let _ = lash::persistence::ProcessRecord::from_registration_with_clock;
    // W0236: lash::persistence::ProcessRegistry::add_observer [function]
    fn meth_0236<T: lash::persistence::ProcessRegistry>(_: &T) {
        let _ = T::add_observer;
    }
    // W0237: lash::persistence::ProcessRegistry::append_event_with_authority [function]
    fn meth_0237<T: lash::persistence::ProcessRegistry>(_: &T) {
        let _ = T::append_event_with_authority;
    }
    // W0239: lash::persistence::ProcessRegistry::clear_process_wait_with_authority [function]
    fn meth_0239<T: lash::persistence::ProcessRegistry>(_: &T) {
        let _ = T::clear_process_wait_with_authority;
    }
    // W0242: lash::persistence::ProcessRegistry::delete_session_process_state [function]
    fn meth_0242<T: lash::persistence::ProcessRegistry>(_: &T) {
        let _ = T::delete_session_process_state;
    }
    // W0245: lash::persistence::ProcessRegistry::get_process [function]
    fn meth_0245<T: lash::persistence::ProcessRegistry>(_: &T) {
        let _ = T::get_process;
    }
    // W0246: lash::persistence::ProcessRegistry::list_live_observed_by [function]
    fn meth_0246<T: lash::persistence::ProcessRegistry>(_: &T) {
        let _ = T::list_live_observed_by;
    }
    // W0247: lash::persistence::ProcessRegistry::list_observed_by [function]
    fn meth_0247<T: lash::persistence::ProcessRegistry>(_: &T) {
        let _ = T::list_observed_by;
    }
    // W0250: lash::persistence::ProcessRegistry::processes_changed_since [function]
    fn meth_0250<T: lash::persistence::ProcessRegistry>(_: &T) {
        let _ = T::processes_changed_since;
    }
    // W0252: lash::persistence::ProcessRegistry::record_first_started_with_authority [function]
    fn meth_0252<T: lash::persistence::ProcessRegistry>(_: &T) {
        let _ = T::record_first_started_with_authority;
    }
    // W0255: lash::persistence::ProcessRegistry::set_process_wait_with_authority [function]
    fn meth_0255<T: lash::persistence::ProcessRegistry>(_: &T) {
        let _ = T::set_process_wait_with_authority;
    }
    // W0258: lash::persistence::ProcessRegistry::with_runtime_clock [function]
    fn meth_0258<T: lash::persistence::ProcessRegistry>(_: &T) {
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
    // W0296: lash::process::ProcessStartRequest::definition [function]
    let _ = lash::process::ProcessStartRequest::definition(
        todo!(),
        None,
        serde_json::Map::new(),
        todo!() as lash::process::EngineLaunch,
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

/// FIG-6034 laws 3 and 9 on SQLite memory: a process builds only the
/// components its environment names, in env order, and an environment
/// with ambient tools is refused wherever it is stated.
#[cfg(all(feature = "sqlite", feature = "rlm", feature = "worker"))]
#[allow(
    clippy::expect_used,
    reason = "a law's setup failing is the law failing"
)]
mod named_component_laws {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use lash::plugins::{AfterToolContributions, FormatVersion, HookKey, PluginError};
    use lash::process::{
        ProcessComponentContext, ProcessComponentDeclaration, ProcessComponentEntry,
        ProcessComponentFactory, ProcessComponentSpec, ProcessConfigKey, ProcessEnv,
        ProcessEnvRefusal,
    };
    use lash::tools::ToolDefinitionBindingExt as _;
    use lash_sansio::sync::MutexExt as _;

    const MODEL: &str = "named-components";
    /// Cross-cutting: no tools, one result check.
    const CHECK: ProcessConfigKey<()> = ProcessConfigKey::new("evidence-check", FormatVersion::ONE);
    /// The tool's component, with a result check of its own.
    const A: ProcessConfigKey<()> = ProcessConfigKey::new("evidence-a", FormatVersion::ONE);
    /// Installed and never named.
    const B: ProcessConfigKey<()> = ProcessConfigKey::new("evidence-b", FormatVersion::ONE);
    /// A presenter each, and a second owner of A's tool name.
    const PRESENTER_1: ProcessConfigKey<()> =
        ProcessConfigKey::new("evidence-presenter-1", FormatVersion::ONE);
    const PRESENTER_2: ProcessConfigKey<()> =
        ProcessConfigKey::new("evidence-presenter-2", FormatVersion::ONE);
    const TWIN: ProcessConfigKey<()> = ProcessConfigKey::new("evidence-twin", FormatVersion::ONE);
    const A_TOOL: &str = "evidence_a_tool";
    const ISOLATED_TOOL: &str = "evidence_isolated";

    /// What every component and plugin of the deployment did.
    #[derive(Default)]
    struct Witness {
        /// Builds per component id.
        builds: Mutex<BTreeMap<String, usize>>,
        /// The result checks that ran, by owner, in order.
        hooks: Mutex<Vec<String>>,
        /// Session plugin builds for a process owner.
        process_plugin_builds: AtomicUsize,
    }

    impl Witness {
        fn builds(&self, key: ProcessConfigKey<()>) -> usize {
            self.builds
                .lock_recover()
                .get(key.component())
                .copied()
                .unwrap_or_default()
        }

        fn take_hooks(&self) -> Vec<String> {
            std::mem::take(&mut *self.hooks.lock_recover())
        }
    }

    fn a_definition(name: &str) -> lash::tools::ToolDefinition {
        let object = serde_json::json!({ "type": "object", "additionalProperties": true });
        lash::tools::ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            "Answers `a`.",
            object.clone(),
            object,
        )
        .expect("the tool's schemas")
        .with_execution(std::time::Duration::from_secs(60))
        .with_tool_binding(lash::tools::ToolBinding::new(["tools"], name))
    }

    struct AnswerA;

    #[async_trait::async_trait]
    impl lash::tools::StaticToolExecute for AnswerA {
        async fn execute(
            &self,
            _call: lash::tools::ToolCall<'_>,
        ) -> lash::tools::ToolAttemptOutcome {
            lash::tools::ToolOutcome::ok(serde_json::json!("a")).into()
        }
    }

    fn a_provider() -> Arc<dyn lash::tools::ToolProvider> {
        Arc::new(lash::tools::StaticToolProvider::new(
            vec![a_definition(A_TOOL)],
            AnswerA,
        ))
    }

    /// What one counting component contributes.
    #[derive(Clone, Copy)]
    enum Shape {
        Check,
        Tool,
        Nothing,
        Presenter,
    }

    struct Counting {
        key: ProcessConfigKey<()>,
        shape: Shape,
        witness: Arc<Witness>,
    }

    impl ProcessComponentFactory for Counting {
        fn declaration(&self) -> ProcessComponentDeclaration {
            ProcessComponentDeclaration::initial(self.key.component(), self.key.format())
                .expect("the component id")
        }

        fn config_type(&self) -> std::any::TypeId {
            std::any::TypeId::of::<()>()
        }

        fn validate(
            &self,
            entry: &ProcessComponentEntry,
            _env: &ProcessEnv,
        ) -> Result<(), ProcessEnvRefusal> {
            entry
                .config()
                .decode::<()>()
                .map_err(|error| ProcessEnvRefusal::ConfigRefused {
                    id: entry.id().to_string(),
                    reason: error.to_string(),
                })
        }

        fn build(
            &self,
            _cx: &ProcessComponentContext<'_>,
        ) -> Result<ProcessComponentSpec, PluginError> {
            *self
                .witness
                .builds
                .lock_recover()
                .entry(self.key.component().to_owned())
                .or_default() += 1;
            let id = self.key.component().to_owned();
            let witness = Arc::clone(&self.witness);
            let check: lash::plugins::ToolResultCheckHook = Arc::new(move |_input| {
                witness.hooks.lock_recover().push(id.clone());
                Box::pin(async { Ok(AfterToolContributions::default()) })
            });
            let key = HookKey::new("record").expect("a hook key");
            let mut spec = ProcessComponentSpec::default();
            match self.shape {
                Shape::Check => spec.tool_result_checks.push((key, check)),
                Shape::Tool => {
                    spec.tool_providers.push(a_provider());
                    spec.tool_result_checks.push((key, check));
                }
                Shape::Presenter => {
                    spec.presenter = Some(Arc::new(|input| {
                        Box::pin(async move { Ok(input.previous) })
                    }));
                }
                Shape::Nothing => {}
            }
            Ok(spec)
        }
    }

    /// A session plugin that counts the builds made for a process.
    struct CountingPlugin(Arc<Witness>);

    impl lash::plugins::PluginDefinition for CountingPlugin {
        fn declaration() -> lash::plugins::PluginDeclaration {
            lash::plugins::PluginDeclaration::initial("evidence-counting-plugin")
        }
    }

    struct Inert;

    impl lash::plugins::SessionPlugin for Inert {
        fn id(&self) -> &'static str {
            "evidence-counting-plugin"
        }

        fn register(&self, _reg: &mut lash::plugins::PluginRegistrar) -> Result<(), PluginError> {
            Ok(())
        }
    }

    impl lash::plugins::PluginFactory for CountingPlugin {
        fn id(&self) -> &'static str {
            "evidence-counting-plugin"
        }

        fn build(
            &self,
            ctx: &lash::plugins::PluginSessionContext,
        ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, PluginError> {
            if matches!(ctx.owner, lash_core::RuntimeOwner::Process(_)) {
                self.0.process_plugin_builds.fetch_add(1, Ordering::SeqCst);
            }
            Ok(Arc::new(Inert))
        }
    }

    /// The environment a policy states: the turn's run with `keys`, or
    /// one with ambient tools.
    #[derive(Clone)]
    enum Stated {
        Named(Vec<ProcessConfigKey<()>>),
        Ambient,
    }

    struct Policy(Arc<Mutex<Stated>>);

    #[async_trait::async_trait]
    impl lash::process::ProcessEnvPolicy for Policy {
        async fn choose(
            &self,
            cx: &lash::process::ProcessEnvCx<'_>,
        ) -> Result<lash::process::ProcessStartChoice, lash::process::ProcessEnvPolicyError>
        {
            let stated = self.0.lock_recover().clone();
            let env = match stated {
                Stated::Ambient => ambient_env(),
                Stated::Named(keys) => {
                    let (Some(run), Some(offered)) = (cx.run, cx.offered) else {
                        return Err(lash::process::ProcessEnvPolicyError::Fault(
                            "no recorded run".into(),
                        ));
                    };
                    let entries = keys
                        .iter()
                        .map(|key| ProcessComponentEntry::new(*key, &(), []))
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(lash::process::ProcessEnvPolicyError::Refused)?;
                    ProcessEnv::from_run(run, offered, entries)
                        .map_err(lash::process::ProcessEnvPolicyError::Refused)?
                }
            };
            Ok(lash::process::ProcessStartChoice {
                env: lash::process::ProcessEnvChoice::New(env),
                engine_settings: cx.engine_defaults()?,
            })
        }
    }

    fn ambient_env() -> ProcessEnv {
        ProcessEnv::builder(
            lash::plugins::SessionToolAccess::ambient(),
            lash::MaxToolCalls::new(16),
        )
        .build()
        .expect("an ambient environment builds; admission refuses it")
    }

    /// A tool isolated in the held engine whose provider states an
    /// ambient environment.
    struct AmbientIsolated;

    fn isolated_definition() -> lash::tools::ToolDefinition {
        a_definition(ISOLATED_TOOL)
            .isolated_in(lash_core::testing::HELD_PROCESS_ENGINE_KIND)
            .expect("an isolated tool names its engine")
    }

    #[async_trait::async_trait]
    impl lash::tools::ToolProvider for AmbientIsolated {
        fn tool_manifests(&self) -> Vec<lash::tools::ToolManifest> {
            vec![isolated_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<lash::tools::ToolContract>> {
            (name == ISOLATED_TOOL).then(|| Arc::new(isolated_definition().contract()))
        }

        async fn execute(
            &self,
            _call: lash::tools::ToolCall<'_>,
        ) -> lash::tools::ToolAttemptOutcome {
            lash::tools::ToolOutcome::ok(serde_json::json!("ordinary body")).into()
        }

        fn isolated_start(
            &self,
            call: lash_core::IsolatedProcessRequest<'_>,
        ) -> Result<lash_core::IsolatedStart, ProcessEnvRefusal> {
            Ok(lash_core::IsolatedStart {
                payload: call.args.clone(),
                env: lash::process::ProcessEnvChoice::New(ambient_env()),
                engine_settings: None,
            })
        }
    }

    struct World {
        core: lash::LashCore,
        stores: Arc<dyn lash::StoreSet>,
        witness: Arc<Witness>,
        stated: Arc<Mutex<Stated>>,
        cells: Arc<Mutex<VecDeque<String>>>,
    }

    async fn world() -> World {
        let stores: Arc<dyn lash::StoreSet> = Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("SQLite stores"),
        );
        let backend = lash::durable::DurableBackendBuilder::new(Arc::clone(&stores))
            .process_engine(Arc::new(lash_core::testing::HeldProcessEngine))
            .build()
            .expect("the backend assembles");
        let witness = Arc::new(Witness::default());
        let stated = Arc::new(Mutex::new(Stated::Named(Vec::new())));
        let cells: Arc<Mutex<VecDeque<String>>> = Arc::default();
        let queue = Arc::clone(&cells);
        let model = lash::testing::TestProvider::builder()
            .kind(MODEL)
            .complete(move |_request| {
                let code = queue
                    .lock_recover()
                    .pop_front()
                    .unwrap_or_else(|| "await control.finish(\"idle\");".to_owned());
                async move {
                    Ok(lash::provider::LlmResponse {
                        parts: vec![lash::direct::LlmOutputPart::Text {
                            text: format!("<typescript>\n{code}\n</typescript>"),
                            response_meta: None,
                        }],
                        ..Default::default()
                    })
                }
            })
            .build()
            .into_handle();
        let rlm = lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                .build(),
            lash::rlm::CellDialect::typescript(),
        )
        .with_worker_service(lash::vm::WorkerService::default());
        let mut builder = lash::LashCore::rlm_builder(backend, rlm)
            .serve_test_llm_profile(
                model,
                lash::LlmProfileMetadata::builder(MODEL)
                    .cache_retention(lash::provider::CacheRetention::Short)
                    .context_window_tokens(200_000)
                    .build()
                    .expect("the model's metadata"),
            )
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .tools(a_provider())
            .tools(Arc::new(AmbientIsolated))
            .plugin(Arc::new(CountingPlugin(Arc::clone(&witness))))
            .plugin(Arc::new(
                lash::process_controls::SessionProcessAdminPluginFactory::new(
                    |_: &lash::process::StartCx| lash::process::Lifetime::Detached,
                    Arc::new(Policy(Arc::clone(&stated))),
                ),
            ));
        for (key, shape) in [
            (CHECK, Shape::Check),
            (A, Shape::Tool),
            (B, Shape::Nothing),
            (PRESENTER_1, Shape::Presenter),
            (PRESENTER_2, Shape::Presenter),
            (TWIN, Shape::Tool),
        ] {
            builder = builder.process_component(
                Arc::new(Counting {
                    key,
                    shape,
                    witness: Arc::clone(&witness),
                }),
                key,
            );
        }
        let core = builder
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                lash::persistence::LeaseOwnerId::new("named-components"),
                lash::persistence::LeaseIncarnationId::new("named-components-boot"),
            ))
            .expect("the core builds");
        World {
            core,
            stores,
            witness,
            stated,
            cells,
        }
    }

    impl World {
        /// Run one turn in a fresh session `name` whose cell is `code`, and
        /// answer its finished value.
        async fn run(&self, name: &str, code: &str) -> serde_json::Value {
            self.cells.lock_recover().push_back(code.to_owned());
            let session = self
                .core
                .session(lash::SessionId::try_from(name.to_owned()).expect("a session id"))
                .create(lash::SessionCreation::root(
                    lash::plugins::SessionToolAccess::ambient(),
                    lash::SessionSpec::new(
                        MODEL,
                        lash::TurnBudget::Unbounded,
                        lash::MaxToolCalls::new(64),
                    )
                    .no_progress_budget(lash::NoProgressBudget::bounded(12)),
                ))
                .await
                .expect("the session is created");
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                session.send(lash::TurnInput::text(name)).output(),
            )
            .await
            .expect("the turn settles within a minute")
            .expect("the turn answers");
            output
                .finished()
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| panic!("the cell finishes: {output:?}"))
        }

        /// A process a cell starts under `keys`, which calls A's tool once:
        /// the result checks that ran for the call, in order.
        async fn hooks_under(&self, name: &str, keys: &[ProcessConfigKey<()>]) -> Vec<String> {
            *self.stated.lock_recover() = Stated::Named(keys.to_vec());
            self.witness.take_hooks();
            let value = self
                .run(
                    name,
                    &format!(
                        "const body = async () => await tools.{A_TOOL}({{}});\n\
                         const handle = await processes.start({{ definition: body }});\n\
                         await control.finish(await handle);"
                    ),
                )
                .await;
            assert_eq!(value, serde_json::json!("a"), "the process called A's tool");
            self.witness.take_hooks()
        }

        async fn stored(&self, env: &ProcessEnv) -> bool {
            self.stores
                .process_env_store()
                .get_process_env(&env.stable_ref().expect("the env's reference"))
                .await
                .expect("the env store reads")
                .is_some()
        }

        async fn publish(&self, env: &ProcessEnv) -> Result<(), ProcessEnvRefusal> {
            match self
                .core
                .host_artifacts()
                .publish_process_env(&lash::process::HostArtifactPin::mint(), env)
                .await
            {
                Ok(_) => Ok(()),
                Err(lash::EmbedError::Plugin(PluginError::ProcessEnvRefused { refusal })) => {
                    Err(refusal)
                }
                Err(other) => panic!("the publish answers a typed refusal: {other:?}"),
            }
        }
    }

    fn env_of(keys: &[ProcessConfigKey<()>]) -> ProcessEnv {
        keys.iter()
            .fold(
                ProcessEnv::builder(
                    lash::plugins::SessionToolAccess::restricted([a_definition(A_TOOL)])
                        .expect("the tool"),
                    lash::MaxToolCalls::new(16),
                ),
                |builder, key| builder.component(*key, &()),
            )
            .build()
            .expect("the environment builds")
    }

    /// ONLY-NAMED-COMPONENTS (M2, D11, D12).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_process_builds_only_the_components_its_environment_names_in_env_order() {
        let world = world().await;
        let check = CHECK.component().to_owned();
        let a = A.component().to_owned();

        assert_eq!(
            world.hooks_under("named-check-a", &[CHECK, A]).await,
            vec![check.clone(), a.clone()],
            "check's hook runs, before A's own"
        );
        assert!(world.witness.builds(CHECK) > 0 && world.witness.builds(A) > 0);
        assert_eq!(
            world.witness.builds(B),
            0,
            "an unnamed component never builds"
        );
        assert_eq!(
            world.witness.process_plugin_builds.load(Ordering::SeqCst),
            0,
            "no session plugin builds for a process"
        );

        assert_eq!(
            world.hooks_under("named-a-check", &[A, CHECK]).await,
            vec![a.clone(), check.clone()],
            "swapping the order swaps the hooks"
        );
        assert_ne!(
            env_of(&[CHECK, A]).stable_ref().unwrap(),
            env_of(&[A, CHECK]).stable_ref().unwrap(),
            "the order is part of the env's address"
        );
        assert_eq!(
            world.hooks_under("named-a", &[A]).await,
            vec![a],
            "an omitted check does not run"
        );
        assert_eq!(world.witness.builds(B), 0);

        let uninstalled = ProcessConfigKey::<()>::new("evidence-uninstalled", FormatVersion::ONE);
        let unreadable = ProcessConfigKey::<()>::new(A.component(), FormatVersion::new(2).unwrap());
        let malformed = ProcessEnv::builder(
            lash::plugins::SessionToolAccess::restricted(std::iter::empty()).unwrap(),
            lash::MaxToolCalls::new(16),
        )
        .component(
            ProcessConfigKey::<i64>::new(CHECK.component(), FormatVersion::ONE),
            &7,
        )
        .build()
        .unwrap();
        let cases: [(&str, ProcessEnv, fn(&ProcessEnvRefusal) -> bool); 5] = [
            ("uninstalled", env_of(&[uninstalled]), |refusal| {
                matches!(refusal, ProcessEnvRefusal::ComponentNotInstalled { .. })
            }),
            ("malformed config", malformed, |refusal| {
                matches!(refusal, ProcessEnvRefusal::ConfigRefused { .. })
            }),
            (
                "unreadable config format",
                env_of(&[unreadable]),
                |refusal| matches!(refusal, ProcessEnvRefusal::ConfigFormatUnreadable { .. }),
            ),
            (
                "duplicate presenter",
                env_of(&[PRESENTER_1, PRESENTER_2]),
                |refusal| matches!(refusal, ProcessEnvRefusal::DuplicatePresenter { .. }),
            ),
            ("duplicate tool name", env_of(&[A, TWIN]), |refusal| {
                matches!(refusal, ProcessEnvRefusal::DuplicateToolName { .. })
            }),
        ];
        for (case, env, expected) in cases {
            let refusal = world
                .publish(&env)
                .await
                .expect_err(&format!("{case}: the publish refuses"));
            assert!(expected(&refusal), "{case}: {refusal:?}");
            assert!(!world.stored(&env).await, "{case}: nothing is stored");
        }
        world.core.shutdown().await.expect("the core shuts down");
    }

    /// AMBIENT-REFUSED (D7).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_environment_with_ambient_tools_is_refused_wherever_it_is_stated() {
        let world = world().await;
        let ambient = ambient_env();
        assert_eq!(
            world.publish(&ambient).await,
            Err(ProcessEnvRefusal::AmbientTools)
        );
        assert!(!world.stored(&ambient).await, "nothing is stored");

        *world.stated.lock_recover() = Stated::Ambient;
        let value = world
            .run(
                "ambient-starts",
                &format!(
                    "const body = async () => 1;\n\
                     let started; try {{ await processes.start({{ definition: body }}); started = \"started\"; }} catch (e) {{ started = String(e?.message ?? e); }}\n\
                     let isolated; try {{ await tools.{ISOLATED_TOOL}({{}}); isolated = \"ran\"; }} catch (e) {{ isolated = String(e?.message ?? e); }}\n\
                     await control.finish({{ started: started, isolated: isolated }});"
                ),
            )
            .await;
        for start in ["started", "isolated"] {
            let answer = value[start].as_str().unwrap_or_default();
            assert!(
                answer.contains("ambient"),
                "the {start} start is the call's tool error: {value}"
            );
        }
        let processes = world
            .core
            .processes()
            .list(
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                },
                std::num::NonZeroUsize::new(4).unwrap(),
                None,
            )
            .await
            .expect("the processes list")
            .processes;
        assert!(processes.is_empty(), "no start registered: {processes:?}");

        let restricted = env_of(&[A]);
        world
            .publish(&restricted)
            .await
            .expect("restricted access passes");
        assert!(world.stored(&restricted).await);
        world.core.shutdown().await.expect("the core shuts down");
    }
}
