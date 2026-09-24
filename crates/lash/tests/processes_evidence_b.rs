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
    // W0326: lash::process::ProcessWakeDelivery::event_type [field]
    field_witness(|value: &lash::process::ProcessWakeDelivery| {
        let _ = &value.event_type;
    });
    // W0327: lash::process::ProcessWakeDelivery::input [field]
    field_witness(|value: &lash::process::ProcessWakeDelivery| {
        let _ = &value.input;
    });
    // W0328: lash::process::ProcessWakeDelivery::process_caused_by [field]
    field_witness(|value: &lash::process::ProcessWakeDelivery| {
        let _ = &value.process_caused_by;
    });
    // W0329: lash::process::ProcessWakeDelivery::process_id [field]
    field_witness(|value: &lash::process::ProcessWakeDelivery| {
        let _ = &value.process_id;
    });
    // W0330: lash::process::ProcessWakeDelivery::sequence [field]
    field_witness(|value: &lash::process::ProcessWakeDelivery| {
        let _ = &value.sequence;
    });
    // W0331: lash::process::ProcessWakeDelivery::target_session_id [field]
    field_witness(|value: &lash::process::ProcessWakeDelivery| {
        let _ = &value.target_session_id;
    });
    // W0332: lash::process::ProcessWakeDelivery::version [field]
    field_witness(|value: &lash::process::ProcessWakeDelivery| {
        let _ = &value.version;
    });
    // W0333: lash::process::ProcessWakeDelivery::wake_id [field]
    field_witness(|value: &lash::process::ProcessWakeDelivery| {
        let _ = &value.wake_id;
    });
    // W0334: lash::process::ProcessWakeSpec [struct]
    type_witness::<lash::process::ProcessWakeSpec>();
    // W0335: lash::process::ProcessWakeSpec::input [field]
    field_witness(|value: &lash::process::ProcessWakeSpec| {
        let _ = &value.input;
    });
    // W0336: lash::process::ProcessWakeSpec::when [field]
    field_witness(|value: &lash::process::ProcessWakeSpec| {
        let _ = &value.when;
    });
    // W0345: lash::process::SessionScopeId [struct]
    type_witness::<lash::process::SessionScopeId>();
    // W0346: lash::process::SessionScopeId::as_str [function]
    let _ = lash::process::SessionScopeId::as_str;
    // W0347: lash::process::SessionScopeId::new [function]
    let _ = lash::process::SessionScopeId::new(String::new());
    // W0348: lash::process::WakeDeliveryDriver::drive_pending_once_with_delivery_policy [function]
    let _ = lash::process::WakeDeliveryDriver::drive_pending_once_with_delivery_policy;
    // W0351: lash::runtime::AwaitEventResolver::await_await_event [function]
    fn meth_0351<T: lash::runtime::AwaitEventResolver>(_: &T) {
        let _ = T::await_await_event;
    }
    // W0352: lash::runtime::AwaitEventResolver::await_event_key [function]
    fn meth_0352<T: lash::runtime::AwaitEventResolver>(_: &T) {
        let _ = T::await_event_key;
    }
    // W0353: lash::runtime::AwaitEventResolver::cancel_await_events_for_session [function]
    fn meth_0353<T: lash::runtime::AwaitEventResolver>(_: &T) {
        let _ = T::cancel_await_events_for_session;
    }
    // W0355: lash::runtime::AwaitEventResolver::peek_await_event [function]
    fn meth_0355<T: lash::runtime::AwaitEventResolver>(_: &T) {
        let _ = T::peek_await_event;
    }
    // W0356: lash::runtime::AwaitEventResolver::resolve_await_event [function]
    fn meth_0356<T: lash::runtime::AwaitEventResolver>(_: &T) {
        let _ = T::resolve_await_event;
    }
    // W0357: lash::runtime::AwaitEventResolver::revoke_await_events_for_session [function]
    fn meth_0357<T: lash::runtime::AwaitEventResolver>(_: &T) {
        let _ = T::revoke_await_events_for_session;
    }
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
    let _ = lash::runtime::ExecutionScope::process(lash::ProcessId::from("x"));
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
    // W0369: lash::runtime::ProcessCommand::List::mode [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::List { mode, .. } = value {
            let _ = mode;
        }
    });
    // W0370: lash::runtime::ProcessCommand::List::session_scope [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::List { session_scope, .. } = value {
            let _ = session_scope;
        }
    });
    // W0371: lash::runtime::ProcessCommand::Signal [variant]
    variant_witness(|value: &lash::runtime::ProcessCommand| {
        matches!(value, lash::runtime::ProcessCommand::Signal { .. })
    });
    // W0373: lash::runtime::ProcessCommand::Signal::request [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::Signal { request, .. } = value {
            let _ = request;
        }
    });
    // W0374: lash::runtime::ProcessCommand::Signal::signal_id [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::Signal { signal_id, .. } = value {
            let _ = signal_id;
        }
    });
    // W0375: lash::runtime::ProcessCommand::Signal::signal_name [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::Signal { signal_name, .. } = value {
            let _ = signal_name;
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
    // W0393: lash::runtime::ProcessEffectOutcome::Signal [variant]
    variant_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        matches!(value, lash::runtime::ProcessEffectOutcome::Signal { .. })
    });
    // W0394: lash::runtime::ProcessEffectOutcome::Signal::event [field]
    field_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        if let lash::runtime::ProcessEffectOutcome::Signal { event, .. } = value {
            let _ = event;
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
    // W0401: lash::runtime::QueuedWorkExecutionConcurrencyError [struct]
    type_witness::<lash::runtime::QueuedWorkExecutionConcurrencyError>();
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
    // W0405: lash::runtime::RuntimeEffectLocalExecutor::await_event [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::await_event;
    // W0406: lash::runtime::RuntimeEffectLocalExecutor::await_event_with_clock [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::await_event_with_clock;
    // W0407: lash::runtime::RuntimeEffectLocalExecutor::into_await_event_options [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::into_await_event_options;
    // W0408: lash::runtime::RuntimeEffectLocalExecutor::into_process [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::into_process;
    // W0409: lash::runtime::RuntimeEffectLocalExecutor::processes [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::processes;
    // W0410: lash::runtime::RuntimeEffectLocalExecutor::take_process_outcome_observer [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::take_process_outcome_observer;
    // W0411: lash::runtime::RuntimeEffectLocalExecutor::with_process_outcome_observer [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::with_process_outcome_observer;
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
    // W0414: lash::runtime::RuntimeEffectOutcome::into_peek_await_event [function]
    let _ = lash::runtime::RuntimeEffectOutcome::into_peek_await_event;
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
    // W0418: lash::testing::TestLocalProcessRegistry::set_process_lease_claim_error [function]
    let _ = lash::testing::TestLocalProcessRegistry::set_process_lease_claim_error;
    // W0419: lash::testing::TestLocalProcessRegistry::set_process_lease_release_error [function]
    let _ = lash::testing::TestLocalProcessRegistry::set_process_lease_release_error;
    // W0420: lash::testing::TestLocalProcessRegistry::set_process_lease_renew_error [function]
    let _ = lash::testing::TestLocalProcessRegistry::set_process_lease_renew_error;
    // W0421: lash::testing::TestLocalProcessRegistry::set_process_read_absent [function]
    let _ = lash::testing::TestLocalProcessRegistry::set_process_read_absent;
    // W0422: lash::testing::TestLocalProcessRegistry::set_process_read_error [function]
    let _ = lash::testing::TestLocalProcessRegistry::set_process_read_error;
    // W0423: lash::testing::TestLocalProcessRegistry::set_process_read_error_after [function]
    let _ = lash::testing::TestLocalProcessRegistry::set_process_read_error_after;
    // W0424: lash::testing::TestLocalProcessRegistry::set_process_read_override [function]
    let _ = lash::testing::TestLocalProcessRegistry::set_process_read_override;
    // W0425: lash::testing::TestLocalProcessRegistry::set_process_terminal_write_error [function]
    let _ = lash::testing::TestLocalProcessRegistry::set_process_terminal_write_error;
    // W0426: lash::testing::TestLocalProcessRegistry::set_process_terminal_write_outcome [function]
    let _ = lash::testing::TestLocalProcessRegistry::set_process_terminal_write_outcome;
    // W0427: lash::testing::TestLocalProcessRegistry::with_clock [function]
    let _ = lash::testing::TestLocalProcessRegistry::with_clock;
    // W0428: lash::testing::TestLocalProcessRegistry::with_wake_delivery_config [function]
    let _ = lash::testing::TestLocalProcessRegistry::with_wake_delivery_config;
    // W0430: lash::tools::ToolContext::emit_child_process_started [function]
    let _ = lash::tools::ToolContext::emit_child_process_started(
        todo!(),
        lash::ProcessId::from("x"),
        todo!(),
        todo!(),
        todo!(),
    );
    // W0431: lash::tools::ToolContext::process_events [function]
    let _ = lash::tools::ToolContext::process_events(todo!());
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
    let _ = lash::durability::EffectJournalRetirement::process(lash::ProcessId::from("x"));
    // W0437: lash::process::ProcessChange [enum]
    type_witness::<lash::process::ProcessChange>();
    // W0438: lash::process::ProcessChange::Deleted [variant]
    variant_witness(|value: &lash::process::ProcessChange| {
        matches!(value, lash::process::ProcessChange::Deleted { .. })
    });
    // W0439: lash::process::ProcessChange::Deleted::tombstone [field]
    field_witness(|value: &lash::process::ProcessChange| {
        if let lash::process::ProcessChange::Deleted { tombstone, .. } = value {
            let _ = tombstone;
        }
    });
    // W0440: lash::process::ProcessChange::Upsert [variant]
    variant_witness(|value: &lash::process::ProcessChange| {
        matches!(value, lash::process::ProcessChange::Upsert { .. })
    });
    // W0441: lash::process::ProcessChange::Upsert::record [field]
    field_witness(|value: &lash::process::ProcessChange| {
        if let lash::process::ProcessChange::Upsert { record, .. } = value {
            let _ = record;
        }
    });
    // W0442: lash::process::ProcessCompletionOutcome [enum]
    type_witness::<lash::process::ProcessCompletionOutcome>();
    // W0443: lash::process::ProcessCompletionOutcome::AlreadyApplied [variant]
    variant_witness(|value: &lash::process::ProcessCompletionOutcome| {
        matches!(
            value,
            lash::process::ProcessCompletionOutcome::AlreadyApplied { .. }
        )
    });
    // W0444: lash::process::ProcessCompletionOutcome::AlreadyApplied::stored [field]
    field_witness(|value: &lash::process::ProcessCompletionOutcome| {
        if let lash::process::ProcessCompletionOutcome::AlreadyApplied { stored, .. } = value {
            let _ = stored;
        }
    });
    // W0445: lash::process::ProcessCompletionOutcome::Committed [variant]
    variant_witness(|value: &lash::process::ProcessCompletionOutcome| {
        matches!(
            value,
            lash::process::ProcessCompletionOutcome::Committed(..)
        )
    });
    // W0446: lash::process::ProcessCompletionOutcome::Committed::0 [field]
    field_witness(|value: &lash::process::ProcessCompletionOutcome| {
        if let lash::process::ProcessCompletionOutcome::Committed(f0) = value {
            let _ = f0;
        }
    });
    // W0447: lash::process::ProcessCompletionOutcome::Superseded [variant]
    variant_witness(|value: &lash::process::ProcessCompletionOutcome| {
        matches!(
            value,
            lash::process::ProcessCompletionOutcome::Superseded { .. }
        )
    });
    // W0448: lash::process::ProcessCompletionOutcome::Superseded::stored [field]
    field_witness(|value: &lash::process::ProcessCompletionOutcome| {
        if let lash::process::ProcessCompletionOutcome::Superseded { stored, .. } = value {
            let _ = stored;
        }
    });
    // W0449: lash::process::ProcessCompletionOutcome::from_stored [function]
    let _ = lash::process::ProcessCompletionOutcome::from_stored;
    // W0450: lash::plugins::ProcessEngine [trait]
    fn trait_witness_0450<T: lash::plugins::ProcessEngine>() {}
    // W0452: lash::plugins::ProcessEngine::kind [function]
    fn meth_0452<T: lash::plugins::ProcessEngine>(_: &T) {
        let _ = T::kind;
    }
    // W0453: lash::plugins::ProcessEngine::run [function]
    fn meth_0453<T: lash::plugins::ProcessEngine>(_: &T) {
        let _ = T::run;
    }
    // W0455: lash_core::ProcessEngineContributionContext [struct]
    type_witness::<lash_core::ProcessEngineContributionContext>();
    // W0456: lash_core::ProcessEngineContributionContext::extensions [function]
    let _ = lash_core::ProcessEngineContributionContext::extensions;
    // W0457: lash_core::ProcessEngineContributionContext::new [function]
    let _ = lash_core::ProcessEngineContributionContext::new;
    // W0458: lash_core::ProcessEngineContributionContext::process_lifecycle_available [function]
    let _ = lash_core::ProcessEngineContributionContext::process_lifecycle_available;
    // W0459: lash_core::ProcessEngineContributionContext::trace_context [function]
    let _ = lash_core::ProcessEngineContributionContext::trace_context;
    // W0460: lash::plugins::ProcessEngineRunContext [struct]
    type_witness::<lash::plugins::ProcessEngineRunContext>();
    // W0461: lash::plugins::ProcessEngineRunContext::cancellation_token [function]
    let _ = lash::plugins::ProcessEngineRunContext::cancellation_token;
    // W0462: lash::plugins::ProcessEngineRunContext::effect_controller [function]
    let _ = lash::plugins::ProcessEngineRunContext::effect_controller;
    // W0463: lash::plugins::ProcessEngineRunContext::execution_context [function]
    let _ = lash::plugins::ProcessEngineRunContext::execution_context;
    // W0464: lash::plugins::ProcessEngineRunContext::into_runtime_context [function]
    let _ = lash::plugins::ProcessEngineRunContext::into_runtime_context;
    // W0465: lash::plugins::ProcessEngineRunContext::named_phase [function]
    let _ = lash::plugins::ProcessEngineRunContext::named_phase;
    // W0466: lash::plugins::ProcessEngineRunContext::plugins [function]
    let _ = lash::plugins::ProcessEngineRunContext::plugins;
    // W0467: lash::plugins::ProcessEngineRunContext::process_registry_available [function]
    let _ = lash::plugins::ProcessEngineRunContext::process_registry_available;
    // W0468: lash::plugins::ProcessEngineRunContext::processes [function]
    let _ = lash::plugins::ProcessEngineRunContext::processes;
    // W0470: lash::plugins::ProcessEngineRunContext::registration [function]
    let _ = lash::plugins::ProcessEngineRunContext::registration;
    // W0471: lash::plugins::ProcessEngineRunContext::resolved_tool_catalog [function]
    let _ = lash::plugins::ProcessEngineRunContext::resolved_tool_catalog;
    // W0472: lash::plugins::ProcessEngineRunContext::scoped_effect_controller [function]
    let _ = lash::plugins::ProcessEngineRunContext::scoped_effect_controller;
    // W0473: lash::plugins::ProcessEngineRunContext::session_id [function]
    let _ = lash::plugins::ProcessEngineRunContext::session_id;
    // W0474: lash::plugins::ProcessEngineRunContext::session_store_factory [function]
    let _ = lash::plugins::ProcessEngineRunContext::session_store_factory;
    // W0475: lash::plugins::ProcessEngineRunContext::store [function]
    let _ = lash::plugins::ProcessEngineRunContext::store;
    // W0476: lash::plugins::ProcessEngineRunContext::take_handover [function]
    let _ = lash::plugins::ProcessEngineRunContext::take_handover;
    // W0477: lash::plugins::ProcessEngineRunContext::turn_phase_probe [function]
    let _ = lash::plugins::ProcessEngineRunContext::turn_phase_probe;
    // W0482: lash::process::ProcessExecutionWriteAuthority [enum]
    type_witness::<lash::process::ProcessExecutionWriteAuthority>();
    // W0483: lash::process::ProcessExecutionWriteAuthority::Invocation [variant]
    variant_witness(|value: &lash::process::ProcessExecutionWriteAuthority| {
        matches!(
            value,
            lash::process::ProcessExecutionWriteAuthority::Invocation { .. }
        )
    });
    // W0484: lash::process::ProcessExecutionWriteAuthority::Invocation::attempt [field]
    field_witness(|value: &lash::process::ProcessExecutionWriteAuthority| {
        if let lash::process::ProcessExecutionWriteAuthority::Invocation { attempt, .. } = value {
            let _ = attempt;
        }
    });
    // W0485: lash::process::ProcessExecutionWriteAuthority::Invocation::execution_id [field]
    field_witness(|value: &lash::process::ProcessExecutionWriteAuthority| {
        if let lash::process::ProcessExecutionWriteAuthority::Invocation { execution_id, .. } =
            value
        {
            let _ = execution_id;
        }
    });
    // W0486: lash::process::ProcessExecutionWriteAuthority::Invocation::process_id [field]
    field_witness(|value: &lash::process::ProcessExecutionWriteAuthority| {
        if let lash::process::ProcessExecutionWriteAuthority::Invocation { process_id, .. } = value
        {
            let _ = process_id;
        }
    });
    // W0487: lash::process::ProcessExecutionWriteAuthority::Invocation::resume_from [field]
    field_witness(|value: &lash::process::ProcessExecutionWriteAuthority| {
        if let lash::process::ProcessExecutionWriteAuthority::Invocation { resume_from, .. } = value
        {
            let _ = resume_from;
        }
    });
    // W0488: lash::process::ProcessExecutionWriteAuthority::Lease [variant]
    variant_witness(|value: &lash::process::ProcessExecutionWriteAuthority| {
        matches!(
            value,
            lash::process::ProcessExecutionWriteAuthority::Lease { .. }
        )
    });
    // W0490: lash::process::ProcessExecutionWriteAuthority::bind_attempt [function]
    let _ = lash::process::ProcessExecutionWriteAuthority::bind_attempt;
    // W0491: lash::process::ProcessExecutionWriteAuthority::invocation [function]
    let _ = lash::process::ProcessExecutionWriteAuthority::invocation(
        lash::ProcessId::from("x"),
        String::new(),
    );
    // W0492: lash::process::ProcessExecutionWriteAuthority::invocation_resume [function]
    let _ = lash::process::ProcessExecutionWriteAuthority::invocation_resume(
        lash::ProcessId::from("x"),
        String::new(),
        todo!(),
    );
    // W0493: lash::process::ProcessExecutionWriteAuthority::invocation_started [function]
    let _ = lash::process::ProcessExecutionWriteAuthority::invocation_started;
    // W0494: lash::process::ProcessExecutionWriteAuthority::lease [function]
    let _ = lash::process::ProcessExecutionWriteAuthority::lease;
    // W0495: lash::process::ProcessExecutionWriteAuthority::permits_owner_bound_resume [function]
    let _ = lash::process::ProcessExecutionWriteAuthority::permits_owner_bound_resume;
    // W0496: lash::process::ProcessExecutionWriteAuthority::validate_invocation_for_start [function]
    let _ = lash::process::ProcessExecutionWriteAuthority::validate_invocation_for_start;
    // W0497: lash::process::ProcessExecutionWriteAuthority::validate_invocation_for_write [function]
    let _ = lash::process::ProcessExecutionWriteAuthority::validate_invocation_for_write;
    // W0498: lash::process::ProcessId [type_alias]
    type_witness::<lash::ProcessId>();
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
    // W0504: lash::plugins::ProcessRunOutcome::SegmentBoundary [variant]
    variant_witness(|value: &lash::plugins::ProcessRunOutcome| {
        matches!(value, lash::plugins::ProcessRunOutcome::SegmentBoundary(..))
    });
    // W0505: lash::plugins::ProcessRunOutcome::SegmentBoundary::0 [field]
    field_witness(|value: &lash::plugins::ProcessRunOutcome| {
        if let lash::plugins::ProcessRunOutcome::SegmentBoundary(f0) = value {
            let _ = f0;
        }
    });
    // W0506: lash::plugins::ProcessRunOutcome::Terminal [variant]
    variant_witness(|value: &lash::plugins::ProcessRunOutcome| {
        matches!(value, lash::plugins::ProcessRunOutcome::Terminal { .. })
    });
    // W0508: lash::plugins::ProcessRunOutcome::Terminal::output [field]
    field_witness(|value: &lash::plugins::ProcessRunOutcome| {
        if let lash::plugins::ProcessRunOutcome::Terminal { output, .. } = value {
            let _ = output;
        }
    });
    // W0509: lash_core::ProcessSpawnProvenance [struct]
    type_witness::<lash_core::ProcessSpawnProvenance>();
    // W0510: lash_core::ProcessSpawnProvenance::originator [field]
    field_witness(|value: &lash_core::ProcessSpawnProvenance| {
        let _ = &value.originator;
    });
    // W0511: lash_core::ProcessSpawnProvenance::wake_session_id [field]
    field_witness(|value: &lash_core::ProcessSpawnProvenance| {
        let _ = &value.wake_session_id;
    });
    // W0512: lash::process::ProcessStartOutcome [enum]
    type_witness::<lash::process::ProcessStartOutcome>();
    // W0513: lash::process::ProcessStartOutcome::AlreadyApplied [variant]
    variant_witness(|value: &lash::process::ProcessStartOutcome| {
        matches!(
            value,
            lash::process::ProcessStartOutcome::AlreadyApplied(..)
        )
    });
    // W0514: lash::process::ProcessStartOutcome::AlreadyApplied::0 [field]
    field_witness(|value: &lash::process::ProcessStartOutcome| {
        if let lash::process::ProcessStartOutcome::AlreadyApplied(f0) = value {
            let _ = f0;
        }
    });
    // W0515: lash::process::ProcessStartOutcome::AlreadyStarted [variant]
    variant_witness(|value: &lash::process::ProcessStartOutcome| {
        matches!(
            value,
            lash::process::ProcessStartOutcome::AlreadyStarted { .. }
        )
    });
    // W0516: lash::process::ProcessStartOutcome::AlreadyStarted::by [field]
    field_witness(|value: &lash::process::ProcessStartOutcome| {
        if let lash::process::ProcessStartOutcome::AlreadyStarted { by, .. } = value {
            let _ = by;
        }
    });
    // W0517: lash::process::ProcessStartOutcome::AlreadyStarted::current [field]
    field_witness(|value: &lash::process::ProcessStartOutcome| {
        if let lash::process::ProcessStartOutcome::AlreadyStarted { current, .. } = value {
            let _ = current;
        }
    });
    // W0518: lash::process::ProcessStartOutcome::AttemptsExhausted [variant]
    variant_witness(|value: &lash::process::ProcessStartOutcome| {
        matches!(
            value,
            lash::process::ProcessStartOutcome::AttemptsExhausted { .. }
        )
    });
    // W0519: lash::process::ProcessStartOutcome::AttemptsExhausted::attempts [field]
    field_witness(|value: &lash::process::ProcessStartOutcome| {
        if let lash::process::ProcessStartOutcome::AttemptsExhausted { attempts, .. } = value {
            let _ = attempts;
        }
    });
    // W0520: lash::process::ProcessStartOutcome::AttemptsExhausted::current [field]
    field_witness(|value: &lash::process::ProcessStartOutcome| {
        if let lash::process::ProcessStartOutcome::AttemptsExhausted { current, .. } = value {
            let _ = current;
        }
    });
    // W0521: lash::process::ProcessStartOutcome::AttemptsExhausted::max_attempts [field]
    field_witness(|value: &lash::process::ProcessStartOutcome| {
        if let lash::process::ProcessStartOutcome::AttemptsExhausted { max_attempts, .. } = value {
            let _ = max_attempts;
        }
    });
    // W0522: lash::process::ProcessStartOutcome::Started [variant]
    variant_witness(|value: &lash::process::ProcessStartOutcome| {
        matches!(value, lash::process::ProcessStartOutcome::Started(..))
    });
    // W0523: lash::process::ProcessStartOutcome::Started::0 [field]
    field_witness(|value: &lash::process::ProcessStartOutcome| {
        if let lash::process::ProcessStartOutcome::Started(f0) = value {
            let _ = f0;
        }
    });
    // W0524: lash::process::ProcessTerminalSpec [struct]
    type_witness::<lash::process::ProcessTerminalSpec>();
    // W0525: lash::process::ProcessTerminalSpec::await_output [field]
    field_witness(|value: &lash::process::ProcessTerminalSpec| {
        let _ = &value.await_output;
    });
    // W0526: lash::process::ProcessTerminalSpec::status [field]
    field_witness(|value: &lash::process::ProcessTerminalSpec| {
        let _ = &value.status;
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
    // W0532: lash::plugins::ProtocolBeforeLlmCallContext::processes [field]
    field_witness(|value: &lash::plugins::ProtocolBeforeLlmCallContext| {
        let _ = &value.processes;
    });
    // W0533: lash::plugins::RuntimeExecutionContext::append_process_event [function]
    let _ = lash::plugins::RuntimeExecutionContext::append_process_event;
    // W0534: lash::plugins::RuntimeExecutionContext::await_process_signal_event [function]
    let _ = lash::plugins::RuntimeExecutionContext::await_process_signal_event;
    // W0535: lash::plugins::RuntimeExecutionContext::captured_process_execution_env_ref [function]
    let _ = lash::plugins::RuntimeExecutionContext::captured_process_execution_env_ref;
    // W0536: lash::plugins::RuntimeExecutionContext::process_handle_json [function]
    let _ = lash::plugins::RuntimeExecutionContext::process_handle_json;
    // W0537: lash::plugins::RuntimeExecutionContext::signal_process_by_id [function]
    let _ = lash::plugins::RuntimeExecutionContext::signal_process_by_id;
    // W0538: lash::plugins::RuntimeExecutionContext::sleep_command [function]
    let _ = lash::plugins::RuntimeExecutionContext::sleep_command;
    // W0539: lash::plugins::RuntimeExecutionContext::start_child_process [function]
    let _ = lash::plugins::RuntimeExecutionContext::start_child_process(
        todo!(),
        todo!(),
        String::new(),
        todo!(),
    );
    // W0540: lash_core::TestProcessRegistryWriteExt [trait]
    fn trait_witness_0540<T: lash_core::TestProcessRegistryWriteExt>() {}
    // W0541: lash_core::TestProcessRegistryWriteExt::clear_process_wait [function]
    fn meth_0541<T: lash_core::TestProcessRegistryWriteExt>(_: &T) {
        let _ = T::clear_process_wait;
    }
    // W0542: lash_core::TestProcessRegistryWriteExt::record_first_started [function]
    fn meth_0542<T: lash_core::TestProcessRegistryWriteExt>(_: &T) {
        let _ = T::record_first_started;
    }
    // W0543: lash_core::TestProcessRegistryWriteExt::set_process_wait [function]
    fn meth_0543<T: lash_core::TestProcessRegistryWriteExt>(_: &T) {
        let _ = T::set_process_wait;
    }
    // W0544: lash::process::WakeDelivery [struct]
    type_witness::<lash::process::WakeDelivery>();
    // W0545: lash::process::WakeDelivery::attempts [field]
    field_witness(|value: &lash::process::WakeDelivery| {
        let _ = &value.attempts;
    });
    // W0546: lash::process::WakeDelivery::delivery_id [field]
    field_witness(|value: &lash::process::WakeDelivery| {
        let _ = &value.delivery_id;
    });
    // W0547: lash::process::WakeDelivery::disposition [field]
    field_witness(|value: &lash::process::WakeDelivery| {
        let _ = &value.disposition;
    });
    // W0548: lash::process::WakeDelivery::expires_at_ms [field]
    field_witness(|value: &lash::process::WakeDelivery| {
        let _ = &value.expires_at_ms;
    });
    // W0549: lash::process::WakeDelivery::first_attempt_ms [field]
    field_witness(|value: &lash::process::WakeDelivery| {
        let _ = &value.first_attempt_ms;
    });
    // W0550: lash::process::WakeDelivery::next_attempt_at_ms [field]
    field_witness(|value: &lash::process::WakeDelivery| {
        let _ = &value.next_attempt_at_ms;
    });
    // W0551: lash::process::WakeDelivery::pending [function]
    let _ = lash::process::WakeDelivery::pending;
    // W0552: lash::process::WakeDelivery::state [function]
    let _ = lash::process::WakeDelivery::state;
    // W0553: lash::process::WakeDelivery::wake [field]
    field_witness(|value: &lash::process::WakeDelivery| {
        let _ = &value.wake;
    });
    // W0554: lash::process::WakeDeliveryBlockedGroup [struct]
    type_witness::<lash::process::WakeDeliveryBlockedGroup>();
    // W0555: lash::process::WakeDeliveryBlockedGroup::blocking_delivery_id [field]
    field_witness(|value: &lash::process::WakeDeliveryBlockedGroup| {
        let _ = &value.blocking_delivery_id;
    });
    // W0556: lash::process::WakeDeliveryBlockedGroup::blocking_sequence [field]
    field_witness(|value: &lash::process::WakeDeliveryBlockedGroup| {
        let _ = &value.blocking_sequence;
    });
    // W0557: lash::process::WakeDeliveryBlockedGroup::process_id [field]
    field_witness(|value: &lash::process::WakeDeliveryBlockedGroup| {
        let _ = &value.process_id;
    });
    // W0558: lash::process::WakeDeliveryBlockedGroup::reason [field]
    field_witness(|value: &lash::process::WakeDeliveryBlockedGroup| {
        let _ = &value.reason;
    });
    // W0559: lash::process::WakeDeliveryBlockedGroup::redrive_delivery_id [field]
    field_witness(|value: &lash::process::WakeDeliveryBlockedGroup| {
        let _ = &value.redrive_delivery_id;
    });
    // W0560: lash::process::WakeDeliveryBlockedGroup::target_session_id [field]
    field_witness(|value: &lash::process::WakeDeliveryBlockedGroup| {
        let _ = &value.target_session_id;
    });
    // W0561: lash::process::WakeDeliveryClaimOutcome [enum]
    type_witness::<lash::process::WakeDeliveryClaimOutcome>();
    // W0562: lash::process::WakeDeliveryClaimOutcome::Applied [variant]
    variant_witness(|value: &lash::process::WakeDeliveryClaimOutcome| {
        matches!(value, lash::process::WakeDeliveryClaimOutcome::Applied)
    });
    // W0563: lash::process::WakeDeliveryClaimOutcome::ClaimLost [variant]
    variant_witness(|value: &lash::process::WakeDeliveryClaimOutcome| {
        matches!(
            value,
            lash::process::WakeDeliveryClaimOutcome::ClaimLost { .. }
        )
    });
    // W0564: lash::process::WakeDeliveryClaimOutcome::ClaimLost::state [field]
    field_witness(|value: &lash::process::WakeDeliveryClaimOutcome| {
        if let lash::process::WakeDeliveryClaimOutcome::ClaimLost { state, .. } = value {
            let _ = state;
        }
    });
    // W0565: lash::process::WakeDeliveryDisposition [enum]
    type_witness::<lash::process::WakeDeliveryDisposition>();
    // W0566: lash::process::WakeDeliveryDisposition::Discarded [variant]
    variant_witness(|value: &lash::process::WakeDeliveryDisposition| {
        matches!(
            value,
            lash::process::WakeDeliveryDisposition::Discarded { .. }
        )
    });
    // W0567: lash::process::WakeDeliveryDisposition::Discarded::reason [field]
    field_witness(|value: &lash::process::WakeDeliveryDisposition| {
        if let lash::process::WakeDeliveryDisposition::Discarded { reason, .. } = value {
            let _ = reason;
        }
    });
    // W0568: lash::process::WakeDeliveryDisposition::DiscardedUnattributed [variant]
    variant_witness(|value: &lash::process::WakeDeliveryDisposition| {
        matches!(
            value,
            lash::process::WakeDeliveryDisposition::DiscardedUnattributed
        )
    });
    // W0569: lash::process::WakeDeliveryDisposition::Enqueued [variant]
    variant_witness(|value: &lash::process::WakeDeliveryDisposition| {
        matches!(value, lash::process::WakeDeliveryDisposition::Enqueued)
    });
    // W0570: lash::process::WakeDeliveryDisposition::Enqueuing [variant]
    variant_witness(|value: &lash::process::WakeDeliveryDisposition| {
        matches!(
            value,
            lash::process::WakeDeliveryDisposition::Enqueuing { .. }
        )
    });
    // W0571: lash::process::WakeDeliveryDisposition::Enqueuing::claim_token [field]
    field_witness(|value: &lash::process::WakeDeliveryDisposition| {
        if let lash::process::WakeDeliveryDisposition::Enqueuing { claim_token, .. } = value {
            let _ = claim_token;
        }
    });
    // W0572: lash::process::WakeDeliveryDisposition::Pending [variant]
    variant_witness(|value: &lash::process::WakeDeliveryDisposition| {
        matches!(value, lash::process::WakeDeliveryDisposition::Pending)
    });
    // W0573: lash::process::WakeDeliveryDisposition::discard_reason [function]
    let _ = lash::process::WakeDeliveryDisposition::discard_reason;
    // W0574: lash::process::WakeDeliveryDisposition::state [function]
    let _ = lash::process::WakeDeliveryDisposition::state;
    // W0575: lash::process::WakeDeliveryReport [struct]
    type_witness::<lash::process::WakeDeliveryReport>();
    // W0576: lash::process::WakeDeliveryReport::blocked_groups [field]
    field_witness(|value: &lash::process::WakeDeliveryReport| {
        let _ = &value.blocked_groups;
    });
    // W0577: lash::process::WakeDeliveryReport::discarded [field]
    field_witness(|value: &lash::process::WakeDeliveryReport| {
        let _ = &value.discarded;
    });
    // W0578: lash::process::WakeDeliveryReport::enqueued [field]
    field_witness(|value: &lash::process::WakeDeliveryReport| {
        let _ = &value.enqueued;
    });
    // W0579: lash::process::WakeDeliveryReport::enqueuing [field]
    field_witness(|value: &lash::process::WakeDeliveryReport| {
        let _ = &value.enqueuing;
    });
    // W0580: lash::process::WakeDeliveryReport::expired [field]
    field_witness(|value: &lash::process::WakeDeliveryReport| {
        let _ = &value.expired;
    });
    // W0581: lash::process::WakeDeliveryReport::from_deliveries [function]
    let _ = lash::process::WakeDeliveryReport::from_deliveries(std::iter::empty::<
        &'static lash::process::WakeDelivery,
    >());
    // W0582: lash::process::WakeDeliveryReport::pending [field]
    field_witness(|value: &lash::process::WakeDeliveryReport| {
        let _ = &value.pending;
    });
    // W0583: lash::process::WakeDeliveryReport::retargeted [field]
    field_witness(|value: &lash::process::WakeDeliveryReport| {
        let _ = &value.retargeted;
    });
    // W0584: lash::process::WakeDeliveryReport::sequence_rewound [field]
    field_witness(|value: &lash::process::WakeDeliveryReport| {
        let _ = &value.sequence_rewound;
    });
    // W0585: lash::process::WakeDeliveryReport::target_gone [field]
    field_witness(|value: &lash::process::WakeDeliveryReport| {
        let _ = &value.target_gone;
    });
    // W0586: lash::process::WakeDeliveryState [enum]
    type_witness::<lash::process::WakeDeliveryState>();
    // W0587: lash::process::WakeDeliveryState::Discarded [variant]
    variant_witness(|value: &lash::process::WakeDeliveryState| {
        matches!(value, lash::process::WakeDeliveryState::Discarded)
    });
    // W0588: lash::process::WakeDeliveryState::Enqueued [variant]
    variant_witness(|value: &lash::process::WakeDeliveryState| {
        matches!(value, lash::process::WakeDeliveryState::Enqueued)
    });
    // W0589: lash::process::WakeDeliveryState::Enqueuing [variant]
    variant_witness(|value: &lash::process::WakeDeliveryState| {
        matches!(value, lash::process::WakeDeliveryState::Enqueuing)
    });
    // W0590: lash::process::WakeDeliveryState::Pending [variant]
    variant_witness(|value: &lash::process::WakeDeliveryState| {
        matches!(value, lash::process::WakeDeliveryState::Pending)
    });
    // W0591: lash::process::WakeDeliveryState::as_str [function]
    let _ = lash::process::WakeDeliveryState::as_str;
    // W0592: lash::process::WakeDiscardReason [enum]
    type_witness::<lash::process::WakeDiscardReason>();
    // W0593: lash::process::WakeDiscardReason::Expired [variant]
    variant_witness(|value: &lash::process::WakeDiscardReason| {
        matches!(value, lash::process::WakeDiscardReason::Expired)
    });
    // W0594: lash::process::WakeDiscardReason::Retargeted [variant]
    variant_witness(|value: &lash::process::WakeDiscardReason| {
        matches!(value, lash::process::WakeDiscardReason::Retargeted)
    });
    // W0595: lash::process::WakeDiscardReason::SequenceRewound [variant]
    variant_witness(|value: &lash::process::WakeDiscardReason| {
        matches!(value, lash::process::WakeDiscardReason::SequenceRewound)
    });
    // W0596: lash::process::WakeDiscardReason::TargetGone [variant]
    variant_witness(|value: &lash::process::WakeDiscardReason| {
        matches!(value, lash::process::WakeDiscardReason::TargetGone)
    });
    // W0597: lash::process::WakeDiscardReason::NON_BLOCKING_ORDERING_GROUP_LABELS [assoc_const]
    let _ = lash::process::WakeDiscardReason::NON_BLOCKING_ORDERING_GROUP_LABELS;
    // W0598: lash::process::WakeDiscardReason::as_str [function]
    let _ = lash::process::WakeDiscardReason::as_str;
    // W0599: lash::process::WakeDiscardReason::blocks_ordering_group [function]
    let _ = lash::process::WakeDiscardReason::blocks_ordering_group;
    // W0600: lash_core::facade_support::AgentFrameRun::into_final_turn [function]
    let _ = lash_core::facade_support::AgentFrameRun::into_final_turn;
    // W0602: lash_core::facade_support::ProtocolTurnOptionsFacadeOps [trait]
    fn trait_witness_0602<T: lash_core::facade_support::ProtocolTurnOptionsFacadeOps>() {}
    // W0603: lash_core::facade_support::ProtocolTurnOptionsFacadeOps::merged_with_override [function]
    fn meth_0603<T: lash_core::facade_support::ProtocolTurnOptionsFacadeOps>(_: &T) {
        let _ = T::merged_with_override;
    }
    // W0604: lash_core::facade_support::TurnContextFacadeOps [trait]
    fn trait_witness_0604<T: lash_core::facade_support::TurnContextFacadeOps>() {}
    // W0605: lash_core::facade_support::TurnOptions::with_local_cancel_origin_hint [function]
    let _ = lash_core::facade_support::TurnOptions::with_local_cancel_origin_hint;
    // W0606: lash_core::facade_support::registry_transitions::RETIRED_PROCESS_STATUS_LABELS [constant]
    let _ = lash_core::facade_support::registry_transitions::RETIRED_PROCESS_STATUS_LABELS;
    // W0607: lash_core::facade_support::registry_transitions::ProcessLeaseRow::project [function]
    let _ = lash_core::facade_support::registry_transitions::ProcessLeaseRow::project;
    // W0608: lash_core::facade_support::registry_transitions::WakeDeliveryRow::project [function]
    let _ = lash_core::facade_support::registry_transitions::WakeDeliveryRow::project;
    // W0609: lash::runtime::RuntimeNamedPhase [struct]
    type_witness::<lash::runtime::RuntimeNamedPhase>();
    // W0610: lash::runtime::RuntimeNamedPhase::begin [function]
    let _ = lash::runtime::RuntimeNamedPhase::begin;
    // W0611: lash::runtime::RuntimeTurnPhaseProbeSlot [struct]
    type_witness::<lash::runtime::RuntimeTurnPhaseProbeSlot>();
    // W0612: lash::runtime::RuntimeTurnPhaseProbeSlot::get_for_scope [function]
    let _ = lash::runtime::RuntimeTurnPhaseProbeSlot::get_for_scope;
    // W0613: lash::runtime::RuntimeTurnPhaseProbeSlot::set_for_scope [function]
    let _ = lash::runtime::RuntimeTurnPhaseProbeSlot::set_for_scope;
    // W0614: lash::runtime::RuntimeTurnPhaseProbeSlot::set_for_session [function]
    let _ = lash::runtime::RuntimeTurnPhaseProbeSlot::set_for_session(
        todo!(),
        lash::SessionId::from("x"),
        todo!(),
    );
    // W0615: lash::runtime::RuntimeControlConfig::process_wake_delivery_policy [field]
    field_witness(|value: &lash::runtime::RuntimeControlConfig| {
        let _ = &value.process_wake_delivery_policy;
    });
    // W0616: lash::plugins::ProcessEngineProcessContext [struct]
    type_witness::<lash::plugins::ProcessEngineProcessContext>();
    // W0617: lash::plugins::ProcessEngineProcessContext::await_terminal [function]
    let _ = lash::plugins::ProcessEngineProcessContext::await_terminal;
    // W0618: lash::plugins::ProcessEngineProcessContext::clear_wait [function]
    let _ = lash::plugins::ProcessEngineProcessContext::clear_wait;
    // W0619: lash::plugins::ProcessEngineProcessContext::emit [function]
    let _ = lash::plugins::ProcessEngineProcessContext::emit;
    // W0621: lash::plugins::ProcessEngineProcessContext::record [function]
    let _ = lash::plugins::ProcessEngineProcessContext::record;
    // W0622: lash::plugins::ProcessEngineProcessContext::set_wait [function]
    let _ = lash::plugins::ProcessEngineProcessContext::set_wait;
    // W0623: lash::plugins::ProcessEngineRegistry [struct]
    type_witness::<lash::plugins::ProcessEngineRegistry>();
    // W0624: lash::plugins::ProcessEngineRegistry::new [function]
    let _ = lash::plugins::ProcessEngineRegistry::new;
    // W0625: lash::plugins::ProcessEngineRegistry::require [function]
    let _ = lash::plugins::ProcessEngineRegistry::require;
    // W0627: lash::plugins::ProcessEngineRunGuard [struct]
    type_witness::<lash::plugins::ProcessEngineRunGuard>();
    // W0628: lash::plugins::ProcessEngineRunGuard::shutdown [function]
    let _ = lash::plugins::ProcessEngineRunGuard::shutdown;
    // W0629: lash::plugins::ProcessEngineRuntimeContext [struct]
    type_witness::<lash::plugins::ProcessEngineRuntimeContext>();
    // W0630: lash::plugins::ProcessEngineRuntimeContext::context [function]
    let _ = lash::plugins::ProcessEngineRuntimeContext::context;
    // W0631: lash::plugins::ProcessEngineRuntimeContext::into_parts [function]
    let _ = lash::plugins::ProcessEngineRuntimeContext::into_parts;
    // W0632: lash::plugins::ProcessEngineRuntimeContext::shutdown [function]
    let _ = lash::plugins::ProcessEngineRuntimeContext::shutdown;
    // W0633: lash::process::ProcessEventSemantics [struct]
    type_witness::<lash::process::ProcessEventSemantics>();
    // W0634: lash::process::ProcessEventSemantics::terminal [field]
    field_witness(|value: &lash::process::ProcessEventSemantics| {
        let _ = &value.terminal;
    });
    // W0635: lash::process::ProcessEventSemantics::wake [field]
    field_witness(|value: &lash::process::ProcessEventSemantics| {
        let _ = &value.wake;
    });
    // W0636: lash::process::ProcessTerminalSemantics [struct]
    type_witness::<lash::process::ProcessTerminalSemantics>();
    // W0637: lash::process::ProcessTerminalSemantics::outcome [field]
    field_witness(|value: &lash::process::ProcessTerminalSemantics| {
        let _ = &value.outcome;
    });
    // W0638: lash::process::ProcessTerminalSemantics::status [field]
    field_witness(|value: &lash::process::ProcessTerminalSemantics| {
        let _ = &value.status;
    });
    // W0639: lash::process::ProcessExecutionConcurrencyError [struct]
    type_witness::<lash::process::ProcessExecutionConcurrencyError>();
    // W0640: lash::persistence::QueuedCheckpointTurnInput [struct]
    type_witness::<lash::persistence::QueuedCheckpointTurnInput>();
    // W0641: lash::persistence::QueuedCheckpointTurnInput::messages [field]
    field_witness(|value: &lash::persistence::QueuedCheckpointTurnInput| {
        let _ = &value.messages;
    });
    // W0642: lash::persistence::QueuedCheckpointTurnInput::turn_causes [field]
    field_witness(|value: &lash::persistence::QueuedCheckpointTurnInput| {
        let _ = &value.turn_causes;
    });
    // W0643: lash::persistence::ProcessWakeSource [struct]
    type_witness::<lash::persistence::ProcessWakeSource>();
    // W0644: lash::persistence::ProcessWakeSource::process_id [field]
    field_witness(|value: &lash::persistence::ProcessWakeSource| {
        let _ = &value.process_id;
    });
    // W0645: lash::persistence::ProcessWakeSource::sequence [field]
    field_witness(|value: &lash::persistence::ProcessWakeSource| {
        let _ = &value.sequence;
    });
    // W0646: lash::persistence::QueuedCheckpointWork [struct]
    type_witness::<lash::persistence::QueuedCheckpointWork>();
    // W0649: lash::persistence::QueuedCheckpointWork::turn_causes [field]
    field_witness(|value: &lash::persistence::QueuedCheckpointWork| {
        let _ = &value.turn_causes;
    });
    // W0650: lash::persistence::QueuedTurnWork [struct]
    type_witness::<lash::persistence::QueuedTurnWork>();
    // W0651: lash::persistence::QueuedTurnWork::input [field]
    field_witness(|value: &lash::persistence::QueuedTurnWork| {
        let _ = &value.input;
    });
    // W0653: lash::persistence::QueuedTurnWork::turn_causes [field]
    field_witness(|value: &lash::persistence::QueuedTurnWork| {
        let _ = &value.turn_causes;
    });
    // W0654: lash::process::WakeDelivery::claim_token [function]
    let _ = lash::process::WakeDelivery::claim_token;
    // W0655: lash::plugins::PluginError::ProcessWorklistCursorBackendMismatch [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::ProcessWorklistCursorBackendMismatch { .. }
        )
    });
    // W0656: lash::plugins::PluginError::ProcessWorklistCursorBackendMismatch::actual [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessWorklistCursorBackendMismatch { actual, .. } =
            value
        {
            let _ = actual;
        }
    });
    // W0657: lash::plugins::PluginError::ProcessWorklistCursorBackendMismatch::expected [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessWorklistCursorBackendMismatch {
            expected, ..
        } = value
        {
            let _ = expected;
        }
    });
    // W0659: lash::process::ProcessService::emit_event [function]
    fn meth_0659<T: lash::process::ProcessService>(_: &T) {
        let _ = T::emit_event;
    }
    // W0660: lash::process::ProcessService::list_visible_for_attempt [function]
    fn meth_0660<T: lash::process::ProcessService>(_: &T) {
        let _ = T::list_visible_for_attempt;
    }
    // W0661: lash::runtime::ProcessCommand::EmitEvent [variant]
    variant_witness(|value: &lash::runtime::ProcessCommand| {
        matches!(value, lash::runtime::ProcessCommand::EmitEvent { .. })
    });
    // W0662: lash::runtime::ProcessCommand::EmitEvent::process_id [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::EmitEvent { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // W0663: lash::runtime::ProcessCommand::EmitEvent::request [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::EmitEvent { request, .. } = value {
            let _ = request;
        }
    });
    // W0664: lash::runtime::ProcessEffectOutcome::EmitEvent [variant]
    variant_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        matches!(value, lash::runtime::ProcessEffectOutcome::EmitEvent { .. })
    });
    // W0665: lash::runtime::ProcessEffectOutcome::EmitEvent::event [field]
    field_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        if let lash::runtime::ProcessEffectOutcome::EmitEvent { event, .. } = value {
            let _ = event;
        }
    });
    // W0673: lash::runtime::ProcessEffectOutcome::EmitEvent::wake_delivery [field]
    field_witness(|value: &lash::runtime::ProcessEffectOutcome| {
        if let lash::runtime::ProcessEffectOutcome::EmitEvent { wake_delivery, .. } = value {
            let _ = wake_delivery;
        }
    });
    // W0676: lash::runtime::RuntimeEffectKind::ToolParentEnd [variant]
    variant_witness(|value: &lash::runtime::RuntimeEffectKind| {
        matches!(value, lash::runtime::RuntimeEffectKind::ToolParentEnd)
    });
    // W0682: lash::process::ProcessRegistry::list_pending_parent_end_plans [function]
    fn meth_0682<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::list_pending_parent_end_plans;
    }
    // W0686: lash::plugins::ProcessRunOutcome::is_terminal [function]
    let _ = lash::plugins::ProcessRunOutcome::is_terminal;
    // W0687: lash::plugins::ProcessRunOutcome::terminal_output [function]
    let _ = lash::plugins::ProcessRunOutcome::terminal_output;
    // W0690: lash::runtime::ProcessCommand::Start::env_spec [field]
    field_witness(|value: &lash::runtime::ProcessCommand| {
        if let lash::runtime::ProcessCommand::Start { env_spec, .. } = value {
            let _ = env_spec;
        }
    });
    // W0691: lash::runtime::RuntimeEffectLocalExecutor::with_process_env_store [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::with_process_env_store;
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
    // W0695: lash::tools::ToolIntentSubmissionRecord::kind [field]
    field_witness(|value: &lash::tools::ToolIntentSubmissionRecord| {
        let _ = &value.kind;
    });
    // W0696: lash::tools::ToolIntentSubmissionRecord::payload_hash [field]
    field_witness(|value: &lash::tools::ToolIntentSubmissionRecord| {
        let _ = &value.payload_hash;
    });
    // W0697: lash::tools::ToolIntentSubmissionRecord::intent [field]
    field_witness(|value: &lash::tools::ToolIntentSubmissionRecord| {
        let _ = &value.intent;
    });
    // W0698: lash::tools::ToolIntentSubmissionRecord::outcome [field]
    field_witness(|value: &lash::tools::ToolIntentSubmissionRecord| {
        let _ = &value.outcome;
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
    // W0705: lash::process::ProcessRegistry::admit_tool_intent_submission [function]
    fn meth_0705<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::admit_tool_intent_submission;
    }
    // W0706: lash::process::ProcessRegistry::complete_tool_intent_submission [function]
    fn meth_0706<T: lash::process::ProcessRegistry>(_: &T) {
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
    let _ = lash_core::facade_support::SessionObserverIntent::host_requested("x");
    // W0717: lash_core::facade_support::SessionObserverIntent::process_id [field]
    field_witness(|value: &lash_core::facade_support::SessionObserverIntent| {
        let _ = &value.process_id;
    });
    // W0718: lash_core::facade_support::SessionObserverIntent::process_incarnation [field]
    field_witness(|value: &lash_core::facade_support::SessionObserverIntent| {
        let _ = &value.process_incarnation;
    });
    // W0722: lash::plugins::PluginError::ProcessCallerDeparted [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::ProcessCallerDeparted { .. }
        )
    });
    // W0723: lash::plugins::PluginError::ProcessCallerDeparted::process_id [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessCallerDeparted { process_id, .. } = value {
            let _ = process_id;
        }
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
    // W0726: lash::process::ProcessEventAppendRequest::caller_departed [function]
    let _ = lash::process::ProcessEventAppendRequest::caller_departed;
    // W0727: lash::process::ProcessService::report_caller_departure [function]
    fn meth_0727<T: lash::process::ProcessService>(_: &T) {
        let _ = T::report_caller_departure;
    }
    // W0728: lash::process::ProcessStatus::is_retired [function]
    let _ = lash::process::ProcessStatus::is_retired;
    // W0730: lash::remote::processes::RemoteProcessStatus::CallerDeparted [variant]
    variant_witness(|value: &lash::remote::processes::RemoteProcessStatus| {
        matches!(
            value,
            lash::remote::processes::RemoteProcessStatus::CallerDeparted
        )
    });
    // W0732: lash::durability::ProcessLocalExecution::execute [function]
    let _ = lash::durability::ProcessLocalExecution::execute;
    // W0733: lash::durability::TriggerLocalExecution::execute [function]
    let _ = lash::durability::TriggerLocalExecution::execute;
}
