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

use lash_core::plugin::PluginRecipe;
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
    // W0193: lash::process::ProcessExecutionContext::is_empty [function]
    let _ = lash::process::ProcessExecutionContext::is_empty;
    // W0194: lash::process::ProcessExecutionContext::with_causal_invocation [function]
    let _ = lash::process::ProcessExecutionContext::with_causal_invocation;
    // W0196: lash::process::ProcessEnv::from_store_bytes [function]
    let _ = lash::process::ProcessEnv::from_store_bytes;
    // W0197: lash::process::ProcessEnv::components [function]
    let _ = lash::process::ProcessEnv::components;
    // W0198: lash::process::ProcessEnv::effects [function]
    let _ = lash::process::ProcessEnv::effects;
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
    // W0279: lash::persistence::ProcessOpScope::execution_context [function]
    let _ = lash::persistence::ProcessOpScope::execution_context;
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

/// FIG-6034 law 3 on SQLite memory: a process builds only the components
/// its environment names, in env order; and the authority read reports the
/// env's linked effects.
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
    use lash_protocol_types::sync::MutexExt as _;

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

    /// What every component and plugin of the deployment did.
    #[derive(Default)]
    struct Witness {
        /// Builds per component id.
        builds: Mutex<BTreeMap<String, usize>>,
        /// The result checks that ran, by owner, in order.
        hooks: Mutex<Vec<String>>,
        /// Session plugin builds for a process owner.
        process_plugin_builds: AtomicUsize,
        offered: Mutex<Option<Arc<lash::plugins::ToolCatalog>>>,
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

    fn operation(text: &str) -> lash::tools::QualifiedOperation {
        lash::tools::IntoOperation::into_operation(text).expect("a canonical operation")
    }

    fn a_definition(name: &str) -> lash::tools::ToolDefinition {
        let object = serde_json::json!({ "type": "object", "additionalProperties": true });
        lash::tools::ToolDefinition::raw(
            format!("tool:{name}"),
            format!("tools.{name}"),
            "Answers `a`.",
            object,
            serde_json::json!({ "type": "string" }),
        )
        .expect("the tool's schemas")
        .with_execution(std::time::Duration::from_secs(60))
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
        authority: bool,
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

        fn recipe(
            self: Arc<Self>,
        ) -> Result<lash::plugins::ProcessRecipe, lash_execution_contracts::plugin_error::PluginError>
        {
            let mut reg =
                lash::plugins::PluginRegistrar::<Self, lash::plugins::registries::Process>::for_process(self.declaration().id.as_str());
            let key = lash::hook_key!("record");
            let check = |this: &Self, _: &lash::plugins::PluginStateView| {
                let id = this.key.component().to_owned();
                let witness = Arc::clone(&this.witness);
                let hook: lash::plugins::ToolResultCheckHook = Arc::new(move |_input| {
                    witness.hooks.lock_recover().push(id.clone());
                    Box::pin(async { Ok(AfterToolContributions::default()) })
                });
                Ok(hook)
            };
            match self.shape {
                Shape::Check => {
                    reg.tool_calls().check_result(key, check)?;
                }
                Shape::Tool => {
                    let provider = if self.authority {
                        lash::tool_provider_key!("tools")
                    } else {
                        lash::tool_provider_key!("record")
                    };
                    reg.tools().provider(provider, |_, _| Ok(a_provider()))?;
                    reg.tool_calls().check_result(key, check)?;
                }
                Shape::Presenter => {
                    reg.tool_results().presenter(|_, _| {
                        Ok(Arc::new(|input| {
                            Box::pin(async move { Ok(input.previous) })
                        }))
                    })?;
                }
                Shape::Nothing => {}
            }
            let id = self.key.component();
            Ok(lash::plugins::ProcessRecipe::new(id, reg, move |_| {
                *self
                    .witness
                    .builds
                    .lock_recover()
                    .entry(self.key.component().to_owned())
                    .or_default() += 1;
                Ok(Arc::clone(&self))
            }))
        }
    }

    /// A session plugin that counts the builds made for a process.
    struct CountingPlugin(Arc<Witness>);

    impl CountingPlugin {
        pub fn declaration() -> lash::plugins::PluginDeclaration {
            lash::plugins::PluginDeclaration::initial("evidence-counting-plugin")
        }
    }
    impl lash::plugins::PluginMetadata for CountingPlugin {
        fn plugin_declaration(&self) -> lash::plugins::PluginDeclaration {
            Self::declaration()
        }
    }

    struct Inert(Arc<Witness>);

    impl lash::plugins::SessionPlugin for Inert {
        fn id(&self) -> &'static str {
            "evidence-counting-plugin"
        }
    }
    impl Inert {
        fn declare(
            reg: &mut lash_core::plugin::PluginRegistrar<Self>,
        ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
            reg.prompt().section(
                lash_core::plugin::prompt::PromptSectionSpec::new(
                    lash::prompt::PromptSectionKey::new("authority-offer")
                        .expect("the probe section key"),
                    lash::prompt::PromptPlacement::CurrentContext,
                ),
                |this, _| {
                    let witness = Arc::clone(&this.0);
                    Ok(Arc::new(
                        move |input: &lash_core::plugin::prompt::PromptInput<'_>| {
                            *witness.offered.lock_recover() =
                                Some(Arc::clone(input.offered().catalog()));
                            Ok(lash_core::plugin::prompt::SectionText::Omit)
                        },
                    ))
                },
            )?;
            Ok(())
        }
    }

    impl lash::plugins::PluginFactory for CountingPlugin {
        fn id(&self) -> &'static str {
            "evidence-counting-plugin"
        }

        fn recipe(
            self: Arc<Self>,
        ) -> std::result::Result<
            lash_core::plugin::PluginRecipe,
            lash_execution_contracts::plugin_error::PluginError,
        > {
            let mut reg = lash_core::plugin::PluginRegistrar::<Inert>::new(
                lash_core::plugin::PluginFactory::id(self.as_ref()),
            );
            Inert::declare(&mut reg)?;
            let id = self.id();
            Ok(lash_core::plugin::PluginRecipe::new(
                id,
                reg,
                move |ctx, _state| {
                    if matches!(ctx.owner, lash_core::RuntimeOwner::Process(_)) {
                        self.0.process_plugin_builds.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(Arc::new(Inert(Arc::clone(&self.0))))
                },
            ))
        }
    }

    /// The environment a policy states: the turn's run captured under
    /// `keys`, or the host's narrowing of a `from_run` choice.
    #[derive(Clone)]
    enum Stated {
        Named(Vec<ProcessConfigKey<()>>),
        Authority,
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
                Stated::Authority => {
                    let policy = lash::process::env::policy::from_run(|_| {
                        Ok(vec![ProcessComponentEntry::new(A, &(), [])?])
                    });
                    let choice = policy.choose(cx).await?;
                    let lash::process::ProcessEnvChoice::New(recorded) = choice.env else {
                        panic!("from_run chooses new")
                    };
                    // The host explicitly narrows its from_run choice to a and b.
                    let mut builder = ProcessEnv::builder(recorded.max_tool_calls())
                        .attachments(Arc::clone(recorded.attachments()))
                        .effects(recorded.effects().clone());
                    if let Some(model) = recorded.model() {
                        builder = builder.model(model.clone());
                    }
                    for entry in recorded.components() {
                        builder = builder.entry(entry.clone());
                    }
                    for (slot, capability) in recorded.capabilities() {
                        builder = builder.capability(slot.clone(), capability.clone());
                    }
                    builder
                        .build()
                        .map_err(lash::process::ProcessEnvPolicyError::Refused)?
                }
                Stated::Named(keys) => {
                    let (Some(run), Some(surface)) = (cx.run, cx.surface) else {
                        return Err(lash::process::ProcessEnvPolicyError::Fault(
                            "no recorded run".into(),
                        ));
                    };
                    let entries = keys
                        .iter()
                        .map(|key| ProcessComponentEntry::new(*key, &(), []))
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(lash::process::ProcessEnvPolicyError::Refused)?;
                    cx.capture(run, surface, entries)
                        .await
                        .map_err(lash::process::ProcessEnvPolicyError::Refused)?
                }
            };
            Ok(lash::process::ProcessStartChoice {
                fits: Default::default(),
                env: lash::process::ProcessEnvChoice::New(env),
                engine_settings: cx.engine_defaults()?,
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
        world_with_reads(None).await
    }

    async fn world_with_reads(reads: Option<Arc<AuthorityProbe>>) -> World {
        let stores: Arc<dyn lash::StoreSet> = Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .expect("SQLite stores"),
        );
        let authority = reads.is_some();
        let stores = match reads {
            Some(reads) => lash_core::testing::runtime_helpers::LayeredStores::over(stores)
                .map_process_authority_reads(|inner| Arc::new(CountingAuthority { inner, reads }))
                .into_store_set(),
            None => stores,
        };
        let backend = lash::durable::DurableBackendBuilder::new(Arc::clone(&stores)).config(lash::durable::DurableSettings { owner_retention: Some(lash::durable::OwnerRetentionSettings { idle_evict: std::time::Duration::from_secs(60),
 max_owner_bytes: const { std::num::NonZeroU64::new(64 * 1024 * 1024).unwrap() },
 max_node_bytes: const { std::num::NonZeroU64::new(512 * 1024 * 1024).unwrap() },
 max_owner_dirty_bytes: const { std::num::NonZeroU64::new(32 * 1024 * 1024).unwrap() },
 max_node_dirty_bytes: const { std::num::NonZeroU64::new(256 * 1024 * 1024).unwrap() }, }), ..lash::durable::DurableSettings::standard() })
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
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .plugin(Arc::new(CountingPlugin(Arc::clone(&witness))))
            .plugin(Arc::new(
                lash::process_controls::SessionProcessAdminPluginFactory::new(
                    |_: &lash::process::StartCx| lash::process::Lifetime::Detached,
                    Arc::new(Policy(Arc::clone(&stated))),
                ),
            ));
        if authority {
            builder = builder.plugin(Arc::new(lash::tools::SelectedToolsPlugin::new(
                A.component(),
                a_provider(),
            )));
            builder = builder.tools(
                lash::tool_provider_key!("authority"),
                Arc::new(lash::tools::StaticToolProvider::new(
                    vec![a_definition("b"), a_definition("c")],
                    AnswerA,
                )),
            );
        } else {
            builder = builder.plugin(Arc::new(lash::plugins::StaticPluginFactory::new(
                lash::plugins::PluginDeclaration::initial(A.component()),
                lash::plugins::PluginSpec::new()
                    .with_tool_provider(lash::tool_provider_key!("record"), a_provider()),
            )));
        }
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
                    authority,
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
        // The node projects what it serves from every installed component
        // as it starts: those builds are no process's.
        witness.builds.lock_recover().clear();
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
                .create(lash::SessionCreation::root(lash::SessionSpec::new(
                    MODEL,
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(64),
                    lash::NoProgressBudget::bounded(12),
                )))
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
                        "const body = async () => await tools.evidenceATool({{}});\n\
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
                ProcessEnv::builder(lash::MaxToolCalls::new(16)),
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
        let malformed = ProcessEnv::builder(lash::MaxToolCalls::new(16))
            .component(
                ProcessConfigKey::<i64>::new(CHECK.component(), FormatVersion::ONE),
                &7,
            )
            .build()
            .unwrap();
        let cases: [(&str, ProcessEnv, fn(&ProcessEnvRefusal) -> bool); 4] = [
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

    #[derive(Default)]
    struct AuthorityProbe {
        calls: AtomicUsize,
        override_bytes: Mutex<Option<Option<Vec<u8>>>>,
        override_env_ref: Mutex<Option<lash::process::ProcessEnvRef>>,
    }

    struct CountingAuthority {
        inner: Arc<dyn lash::persistence::ProcessAuthorityReads>,
        reads: Arc<AuthorityProbe>,
    }

    #[async_trait::async_trait]
    impl lash::persistence::ProcessAuthorityReads for CountingAuthority {
        async fn process_authority(
            &self,
            id: &lash::ProcessId,
        ) -> Result<
            Option<(lash::persistence::ProcessRecord, Option<Vec<u8>>)>,
            lash::persistence::StoreError,
        > {
            self.reads.calls.fetch_add(1, Ordering::SeqCst);
            let result = self.inner.process_authority(id).await?;
            Ok(result.map(|(mut record, bytes)| {
                if let Some(env_ref) = self.reads.override_env_ref.lock_recover().clone() {
                    if let lash_execution_contracts::process::ProcessInput::Engine {
                        launch, ..
                    } = Arc::make_mut(&mut record.input)
                    {
                        launch.env = env_ref;
                    }
                }
                let bytes = if record.env_ref().is_some() {
                    self.reads
                        .override_bytes
                        .lock_recover()
                        .clone()
                        .unwrap_or(bytes)
                } else {
                    bytes
                };
                (record, bytes)
            }))
        }
        async fn environment_bytes(
            &self,
            env: &lash::process::ProcessEnvRef,
        ) -> Result<Option<Vec<u8>>, lash::persistence::StoreError> {
            self.reads.calls.fetch_add(1, Ordering::SeqCst);
            let bytes = self.inner.environment_bytes(env).await?;
            Ok(self
                .reads
                .override_bytes
                .lock_recover()
                .clone()
                .unwrap_or(bytes))
        }
    }

    // The host guide's check, compiled with this owning test target.
    async fn may_address_process(
        processes: &lash::process::Processes,
        id: &lash::ProcessId,
        offered: &lash::plugins::ToolCatalog,
    ) -> lash::Result<bool> {
        Ok(match processes.environment(id).await? {
            Some(lash::process::ProcessEnvView {
                authority: lash::process::ProcessAuthority::Engine { env, .. },
                ..
            }) => lash::process::authority::tools_excess(&env, offered).is_empty(),
            _ => false,
        })
    }

    /// ENV-AUTHORITY-READ (M8, D5, D7): observation never composes executable code.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn env_authority_read() {
        let reads = Arc::new(AuthorityProbe::default());
        let world = world_with_reads(Some(Arc::clone(&reads))).await;
        *world.stated.lock_recover() = Stated::Authority;
        world
            .run(
                "authority-session",
                &format!(
                    "const body = async () => await tools.evidenceATool({{}});
             const handle = await processes.start({{definition: body, label: 'authority-child'}});
             await control.finish(await handle);"
                ),
            )
            .await;
        let record = world
            .stores
            .process_registry()
            .list_processes_page(
                &lash::persistence::ProcessListFilter {
                    status: lash::process::ProcessStatusFilter::Any,
                    identity_label: Some("authority-child".into()),
                    ..Default::default()
                },
                std::num::NonZeroUsize::new(16).unwrap(),
                None,
            )
            .await
            .unwrap()
            .records
            .into_iter()
            .next()
            .unwrap();
        let builds = world.witness.builds(A);
        let before = reads.calls.load(Ordering::SeqCst);
        let view = world
            .core
            .processes()
            .environment(&record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reads.calls.load(Ordering::SeqCst) - before,
            1,
            "one backend read"
        );
        assert_eq!(
            world.witness.builds(A),
            builds,
            "no component builds during observation"
        );
        assert_eq!(view.provenance, record.provenance);
        assert!(
            view.provenance.run.is_some(),
            "the originating run is recorded"
        );
        let lash::process::ProcessAuthority::Engine {
            env_ref,
            env,
            engine_settings,
            ..
        } = &view.authority
        else {
            panic!("engine authority")
        };
        assert_eq!(Some(env_ref), record.env_ref());
        assert_eq!(engine_settings.as_ref(), record.engine_settings());
        assert_eq!(
            env.effects()
                .iter()
                .map(|(effect, link)| (effect.to_string(), link.tool.to_string()))
                .collect::<Vec<_>>(),
            [(format!("tools.{A_TOOL}"), format!("tool:{A_TOOL}"))],
            "the env links exactly the effect the process performs"
        );
        let before = reads.calls.load(Ordering::SeqCst);
        assert_eq!(
            world.core.processes().env(env_ref).await.unwrap().as_ref(),
            Some(env.as_ref())
        );
        assert_eq!(
            reads.calls.load(Ordering::SeqCst) - before,
            1,
            "one read by reference"
        );
        let session = world
            .core
            .session("authority-session".into())
            .open()
            .await
            .unwrap();
        let offering_a = world.witness.offered.lock_recover().clone().unwrap();
        assert!(
            may_address_process(&world.core.processes(), &record.id, &offering_a)
                .await
                .unwrap()
        );
        let config = session.admin().config();
        let revision = config.revision().await.unwrap();
        let outcome = config
            .apply(
                lash::config::ConfigWrite::new("remove-linked-tool", revision),
                lash::config::ConfigTransaction::new().then_entry(
                    lash::tools::SetToolSelection {
                        tools: lash::tools::ToolSelection::AllExcept(vec![
                            lash::tools::ToolId::from(format!("tool:{A_TOOL}")),
                        ]),
                    }
                    .to(A.component()),
                ),
            )
            .await
            .unwrap()
            .await_outcome(&config)
            .await
            .unwrap();
        assert!(
            matches!(
                outcome,
                lash::config::ConfigTransactionOutcome::Applied { .. }
            ),
            "the provider narrowing commits: {outcome:?}"
        );
        assert_eq!(config.revision().await.unwrap(), revision + 1);
        *world.witness.offered.lock_recover() = None;
        session
            .send(lash::TurnInput::text("read the narrowed offer"))
            .output()
            .await
            .unwrap();
        let offered = world.witness.offered.lock_recover().clone().unwrap();
        assert!(!offered.has_callable_tool(&operation(&format!("tools.{A_TOOL}"))));
        assert!(offered.has_callable_tool(&operation("tools.b")));
        assert!(offered.has_callable_tool(&operation("tools.c")));
        // A committed session narrowing leaves the recorded env, settings and
        // provenance unchanged, while its linked effect is now excess.
        assert_eq!(
            world
                .core
                .processes()
                .environment(&record.id)
                .await
                .unwrap(),
            Some(view.clone())
        );
        assert_eq!(
            lash::process::authority::tools_excess(env, &offered),
            [lash::process::authority::ExcessLink {
                effect: format!("tools.{A_TOOL}"),
                tool: lash::tools::ToolId::from(format!("tool:{A_TOOL}")),
            }]
            .into_iter()
            .collect()
        );
        assert!(
            !may_address_process(&world.core.processes(), &record.id, &offered)
                .await
                .unwrap()
        );
        assert!(
            may_address_process(&world.core.processes(), &record.id, &offering_a)
                .await
                .unwrap()
        );
        assert_eq!(world.witness.builds(A), builds);
        // The port's shared body also pins session-turn ids, unknown/pruned ids,
        // missing bytes, and capability binding/contract/args round-trip by ref.
        world.core.shutdown().await.unwrap();
        let children = lash_conformance::env_authority_read(world.stores.clone()).await;
        for child in children {
            let lash_execution_contracts::process::ProcessInput::SessionTurn {
                create_request, ..
            } = child.input.as_ref()
            else {
                assert!(matches!(
                    world.core.processes().environment(&child.id).await,
                    Err(lash::EmbedError::Plugin(
                        PluginError::ProcessNoLongerRetained { .. }
                    ))
                ));
                continue;
            };
            let expected = create_request.session_id.clone().unwrap_or_else(|| {
                lash_execution_contracts::process::process_child_session_id(&child.id)
            });
            let before = reads.calls.load(Ordering::SeqCst);
            let view = world
                .core
                .processes()
                .environment(&child.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(reads.calls.load(Ordering::SeqCst) - before, 1);
            assert_eq!(
                view.authority,
                lash::process::ProcessAuthority::SessionTurn { session: expected }
            );
        }
        assert_eq!(world.witness.builds(A), builds);
        let before = reads.calls.load(Ordering::SeqCst);
        assert_eq!(
            world
                .core
                .processes()
                .environment(&lash::ProcessId::fixture("authority-unknown"))
                .await
                .unwrap(),
            None
        );
        assert_eq!(reads.calls.load(Ordering::SeqCst) - before, 1);
        let catalog = world.core.host_artifacts().process_catalog(env).unwrap();
        assert_eq!(
            catalog
                .entries()
                .iter()
                .map(|entry| entry.manifest.tool())
                .collect::<Vec<_>>(),
            [format!("tools.{A_TOOL}")]
        );
        assert!(
            world.witness.builds(A) > builds,
            "catalog preview deliberately builds components"
        );
        let capability = lash::CapabilityRef {
            binding: lash::BindingId::new("binding:promoted"),
            contract: lash::ContractRef::new("integration", 3),
            args: serde_json::json!({"account": "promoted"}),
        };
        let recorded = ProcessEnv::builder(lash::MaxToolCalls::new(2))
            .capability(lash::SlotId::new("integration"), capability.clone())
            .build()
            .unwrap();
        let reference = world
            .core
            .host_artifacts()
            .publish_process_env(&lash::process::HostArtifactPin::mint(), &recorded)
            .await
            .unwrap();
        let before = reads.calls.load(Ordering::SeqCst);
        let restored = world
            .core
            .processes()
            .env(&reference)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reads.calls.load(Ordering::SeqCst) - before, 1);
        assert_eq!(
            restored
                .capabilities()
                .get(&lash::SlotId::new("integration")),
            Some(&capability)
        );
        let builds = world.witness.builds(A);
        *reads.override_bytes.lock_recover() = Some(None);
        assert!(
            matches!(world.core.processes().environment(&record.id).await, Err(lash::EmbedError::ProcessEnvLoad(lash::plugins::ProcessEnvLoadError::Missing(missing))) if missing == *env_ref)
        );
        assert_eq!(world.core.processes().env(env_ref).await.unwrap(), None);
        *reads.override_bytes.lock_recover() = Some(Some(b"{}".to_vec()));
        assert!(
            matches!(world.core.processes().environment(&record.id).await, Err(lash::EmbedError::ProcessEnvLoad(lash::plugins::ProcessEnvLoadError::Mismatched(mismatched))) if mismatched == *env_ref)
        );
        assert!(matches!(
            world.core.processes().env(env_ref).await,
            Err(lash::EmbedError::ProcessEnvLoad(
                lash::plugins::ProcessEnvLoadError::Mismatched(_)
            ))
        ));
        // A matching content address does not imply a readable environment.
        // Both reads must preserve the decoder's cause rather than report a mismatch.
        let invalid = b"{}";
        let invalid_ref = lash_core_store::process_identity::process_env_ref_for_bytes(invalid);
        let cause = ProcessEnv::from_store_bytes(invalid)
            .unwrap_err()
            .to_string();
        *reads.override_env_ref.lock_recover() = Some(invalid_ref.clone());
        for result in [
            world
                .core
                .processes()
                .environment(&record.id)
                .await
                .map(|_| ()),
            world.core.processes().env(&invalid_ref).await.map(|_| ()),
        ] {
            match result {
                Err(lash::EmbedError::ProcessEnvLoad(
                    lash::plugins::ProcessEnvLoadError::Undecodable { env_ref, message },
                )) => {
                    assert_eq!(env_ref, invalid_ref);
                    assert_eq!(message, cause, "the decode cause is retained");
                }
                other => panic!("correctly addressed invalid bytes must be Undecodable: {other:?}"),
            }
        }
        assert_eq!(world.witness.builds(A), builds);
    }

    /// D7 over effects (P7): a linked effect is within the host's offer when
    /// the offer holds the linked tool id. The model-facing name, the
    /// provider that serves it and its contract are not authority (the
    /// contract is judged at each perform); another id is excess.
    #[test]
    fn linked_effects_obey_the_offered_tool_identity() {
        use lash::plugins::ToolCatalog;
        use lash::process::authority::{ExcessLink, tools_excess, tools_within};
        let env = ProcessEnv::builder(lash::MaxToolCalls::new(2))
            .component(A, &())
            .link_tool(
                A.tool_provider(lash::tool_provider_key!("tools")),
                &a_definition("a"),
            )
            .build()
            .unwrap();
        let excess: std::collections::BTreeSet<_> = [ExcessLink {
            effect: "tools.a".to_owned(),
            tool: lash::tools::ToolId::from("tool:a"),
        }]
        .into_iter()
        .collect();

        let offered =
            ToolCatalog::from_tool_definitions(vec![a_definition("a"), a_definition("b")]);
        assert!(tools_within(&env, &offered));
        let mut renamed = a_definition("a");
        renamed.manifest.operation = operation("renamed.a");
        assert!(
            tools_within(&env, &ToolCatalog::from_tool_definitions(vec![renamed])),
            "a new model-facing name is not authority"
        );
        let mut other_id = a_definition("a");
        other_id.manifest.id = "other:a".into();
        assert_eq!(
            tools_excess(&env, &ToolCatalog::from_tool_definitions(vec![other_id])),
            excess,
            "a same-name tool with another id does not offer the link"
        );
        let object = serde_json::json!({ "type": "object", "additionalProperties": true });
        let changed = lash::tools::ToolDefinition::raw(
            "tool:a",
            "tools.a",
            "Answers `a`.",
            serde_json::json!({
                "type": "object",
                "properties": { "x": { "type": "string" } },
                "required": ["x"]
            }),
            object,
        )
        .expect("the tool's schemas")
        .with_execution(std::time::Duration::from_secs(60));
        assert!(
            tools_within(&env, &ToolCatalog::from_tool_definitions(vec![changed])),
            "a changed contract under the same id is judged at the perform, not here"
        );
        assert!(!tools_within(
            &env,
            &ToolCatalog::from_tool_definitions(Vec::new())
        ));
        let session = lash::SessionId::try_from("granted-session").unwrap();
        let capability = lash::CapabilityRef {
            binding: lash::BindingId::new("binding:one"),
            contract: lash::ContractRef::new("integration", 1),
            args: serde_json::json!({"scope": "read"}),
        };
        let slot = lash::SlotId::new("integration");
        let env = ProcessEnv::builder(lash::MaxToolCalls::new(2))
            .component_with_sessions(A, &(), [session.clone()])
            .capability(slot.clone(), capability.clone())
            .build()
            .unwrap();
        let id = env.components()[0].id().clone();
        assert_eq!(
            lash::process::authority::components_excess(&env, &Default::default()),
            vec![id.clone()]
        );
        assert!(
            lash::process::authority::components_excess(&env, &[id.clone()].into_iter().collect())
                .is_empty()
        );
        assert_eq!(
            lash::process::authority::session_grants(&env),
            [(id, [session].into_iter().collect())]
                .into_iter()
                .collect()
        );
        let equal = [(slot.clone(), capability.clone())].into_iter().collect();
        assert!(lash::process::authority::capabilities_excess(&env, &equal).is_empty());
        assert_eq!(
            lash::process::authority::capabilities_excess(&env, &Default::default()),
            equal
        );
        let different = [(
            slot,
            lash::CapabilityRef {
                args: serde_json::json!({"scope": "none"}),
                ..capability
            },
        )]
        .into_iter()
        .collect();
        assert_eq!(
            lash::process::authority::capabilities_excess(&env, &different),
            equal
        );
    }
}

#[cfg(feature = "codemode")]
#[path = "processes_evidence/link_laws.rs"]
#[allow(
    clippy::expect_used,
    reason = "a law's setup failing is the law failing"
)]
mod link_laws;
