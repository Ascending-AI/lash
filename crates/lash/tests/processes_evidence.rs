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
    // W0001: lash::AwaitEventWaitIdentity::ProcessSignal [variant]
    variant_witness(|value: &lash::AwaitEventWaitIdentity| {
        matches!(value, lash::AwaitEventWaitIdentity::ProcessSignal { .. })
    });
    // W0002: lash::AwaitEventWaitIdentity::ProcessSignal::ordinal [field]
    field_witness(|value: &lash::AwaitEventWaitIdentity| {
        if let lash::AwaitEventWaitIdentity::ProcessSignal { ordinal, .. } = value {
            let _ = ordinal;
        }
    });
    // W0003: lash::AwaitEventWaitIdentity::ProcessSignal::process_id [field]
    field_witness(|value: &lash::AwaitEventWaitIdentity| {
        if let lash::AwaitEventWaitIdentity::ProcessSignal { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // W0004: lash::AwaitEventWaitIdentity::ProcessSignal::signal_name [field]
    field_witness(|value: &lash::AwaitEventWaitIdentity| {
        if let lash::AwaitEventWaitIdentity::ProcessSignal { signal_name, .. } = value {
            let _ = signal_name;
        }
    });
    // W0005: lash::AwaitEventWaitIdentity::process_signal [function]
    let _ = lash::AwaitEventWaitIdentity::process_signal("proc", "sig", 1);
    // W0006: lash::durability::DurableProcessWorkerConfig::with_turn_phase_probe_slot [function]
    let _ = lash::durability::DurableProcessWorkerConfig::with_turn_phase_probe_slot;
    // W0007: lash::runtime::RuntimeEffectLocalExecutor::with_process_turn_cancellation [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::with_process_turn_cancellation;
    // W0008: lash::runtime::RuntimeEffectLocalExecutor::with_turn_cancel_observation [function]
    let _ = lash::runtime::RuntimeEffectLocalExecutor::with_turn_cancel_observation;
    // W0009: lash::testing::conformance::WakeDeliveryOrderingGroupFaultInjector::discard_without_reason [function]
    fn meth_0009<T: lash::testing::conformance::WakeDeliveryOrderingGroupFaultInjector>(_: &T) {
        let _ = T::discard_without_reason;
    }
    // W0010: lash::LashCoreBuilder::process_wake_delivery_policy [function]
    let _ = lash::LashCoreBuilder::process_wake_delivery_policy;
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
    // W0015: lash::durability::DurableProcessWorker::drain_owner_bound_work [function]
    let _ = lash::durability::DurableProcessWorker::drain_owner_bound_work;
    // W0016: lash::durability::DurableProcessWorker::drive_pending_processes [function]
    let _ = lash::durability::DurableProcessWorker::drive_pending_processes;
    // W0017: lash::durability::DurableProcessWorker::from_shared_config [function]
    let _ = lash::durability::DurableProcessWorker::from_shared_config;
    // W0018: lash::durability::DurableProcessWorker::new [function]
    let _ = lash::durability::DurableProcessWorker::new;
    // W0019: lash::durability::DurableProcessWorker::request_process_cancel [function]
    let _ = lash::durability::DurableProcessWorker::request_process_cancel;
    // W0020: lash::durability::DurableProcessWorker::run_process_segment_with_scoped_effect_controller [function]
    let _ =
        lash::durability::DurableProcessWorker::run_process_segment_with_scoped_effect_controller;
    // W0021: lash::durability::DurableProcessWorkerConfig [struct]
    type_witness::<lash::durability::DurableProcessWorkerConfig>();
    // W0022: lash::durability::DurableProcessWorkerConfig::from_plugin_factories [function]
    let _: fn(
        Vec<std::sync::Arc<dyn lash::plugins::PluginFactory>>,
        lash::durability::RuntimeHostConfig,
        std::sync::Arc<dyn lash::persistence::SessionStoreFactory>,
        lash::durability::WorkerProcessWork,
        std::sync::Arc<dyn lash::runtime::QueuedWorkSubstrate>,
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
    // W0028: lash::durability::DurableProcessWorkerConfig::process_execution_concurrency [function]
    let _ = lash::durability::DurableProcessWorkerConfig::process_execution_concurrency;
    // W0032: lash::durability::DurableProcessWorkerConfig::runtime_host [field]
    field_witness(|value: &lash::durability::DurableProcessWorkerConfig| {
        let _ = &value.runtime_host;
    });
    // W0033: lash::durability::DurableProcessWorkerConfig::session_policy [field]
    field_witness(|value: &lash::durability::DurableProcessWorkerConfig| {
        let _ = &value.session_policy;
    });
    // W0034: lash::durability::DurableProcessWorkerConfig::session_store_factory [field]
    field_witness(|value: &lash::durability::DurableProcessWorkerConfig| {
        let _ = &value.session_store_factory;
    });
    // W0035: lash::durability::DurableProcessWorkerConfig::validate_process_execution_concurrency [function]
    let _ = lash::durability::DurableProcessWorkerConfig::validate_process_execution_concurrency;
    // W0037: lash::durability::DurableProcessWorkerConfig::with_process_execution_concurrency [function]
    let _ = lash::durability::DurableProcessWorkerConfig::with_process_execution_concurrency;
    // W0040: lash::durability::DurableProcessWorkerConfig::with_session_policy [function]
    let _ = lash::durability::DurableProcessWorkerConfig::with_session_policy;
    // W0041: lash::durability::ProcessDrainDeferred [struct]
    type_witness::<lash::durability::ProcessDrainDeferred>();
    // W0042: lash::durability::ProcessDrainDeferred::disposition [field]
    field_witness(|value: &lash::durability::ProcessDrainDeferred| {
        let _ = &value.disposition;
    });
    // W0043: lash::durability::ProcessDrainDeferred::process_id [field]
    field_witness(|value: &lash::durability::ProcessDrainDeferred| {
        let _ = &value.process_id;
    });
    // W0044: lash::durability::ProcessDrainReport [struct]
    type_witness::<lash::durability::ProcessDrainReport>();
    // W0045: lash::durability::ProcessDrainReport::abandoned [field]
    field_witness(|value: &lash::durability::ProcessDrainReport| {
        let _ = &value.abandoned;
    });
    // W0046: lash::durability::ProcessDrainReport::deferred [field]
    field_witness(|value: &lash::durability::ProcessDrainReport| {
        let _ = &value.deferred;
    });
    // W0047: lash::durability::DurableProcessWorkerConfig::process_event_sink [field]
    field_witness(|value: &lash::durability::DurableProcessWorkerConfig| {
        let _ = &value.process_event_sink;
    });
    // W0048: lash::durability::DurableProcessWorkerConfig::with_process_event_sink [function]
    let _ = lash::durability::DurableProcessWorkerConfig::with_process_event_sink;
    // W0049: lash::process::ProcessAdmissionReport [struct]
    type_witness::<lash::process::ProcessAdmissionReport>();
    // W0050: lash::process::ProcessAdmissionReport::admitted [field]
    field_witness(|value: &lash::process::ProcessAdmissionReport| {
        let _ = &value.admitted;
    });
    // W0051: lash::process::ProcessAdmissionReport::deferred [field]
    field_witness(|value: &lash::process::ProcessAdmissionReport| {
        let _ = &value.deferred;
    });
    // W0052: lash::process::ProcessAdmissionDeferred [struct]
    type_witness::<lash::process::ProcessAdmissionDeferred>();
    // W0053: lash::process::ProcessAdmissionDeferred::process_id [field]
    field_witness(|value: &lash::process::ProcessAdmissionDeferred| {
        let _ = &value.process_id;
    });
    // W0054: lash::process::ProcessAdmissionDeferred::disposition [field]
    field_witness(|value: &lash::process::ProcessAdmissionDeferred| {
        let _ = &value.disposition;
    });
    // W0055: lash::process::ProcessAdmissionIntake [enum]
    type_witness::<lash::process::ProcessAdmissionIntake>();
    // W0056: lash::process::ProcessAdmissionIntake::Scanned [variant]
    variant_witness(|value: &lash::process::ProcessAdmissionIntake| {
        matches!(value, lash::process::ProcessAdmissionIntake::Scanned)
    });
    // W0057: lash::process::ProcessAdmissionIntake::Coalesced [variant]
    variant_witness(|value: &lash::process::ProcessAdmissionIntake| {
        matches!(value, lash::process::ProcessAdmissionIntake::Coalesced)
    });
    // W0058: lash::process::ProcessAdmissionReport::intake [field]
    field_witness(|value: &lash::process::ProcessAdmissionReport| {
        let _ = &value.intake;
    });
    // W0059: lash::durability::ProcessRecoveryAttemptOutcome::ExternallyOwned [variant]
    variant_witness(|value: &lash::durability::ProcessRecoveryAttemptOutcome| {
        matches!(
            value,
            lash::durability::ProcessRecoveryAttemptOutcome::ExternallyOwned
        )
    });
    // W0060: lash::durability::ProcessRecoveryOperation::SubmitRun [variant]
    variant_witness(|value: &lash::durability::ProcessRecoveryOperation| {
        matches!(value, lash::durability::ProcessRecoveryOperation::SubmitRun)
    });
    // W0061: lash::durability::ProcessRecoveryAttemptOutcome [enum]
    type_witness::<lash::durability::ProcessRecoveryAttemptOutcome>();
    // W0062: lash::durability::ProcessRecoveryAttemptOutcome::Absent [variant]
    variant_witness(|value: &lash::durability::ProcessRecoveryAttemptOutcome| {
        matches!(
            value,
            lash::durability::ProcessRecoveryAttemptOutcome::Absent
        )
    });
    // W0063: lash::durability::ProcessRecoveryAttemptOutcome::AlreadyApplied [variant]
    variant_witness(|value: &lash::durability::ProcessRecoveryAttemptOutcome| {
        matches!(
            value,
            lash::durability::ProcessRecoveryAttemptOutcome::AlreadyApplied { .. }
        )
    });
    // W0064: lash::durability::ProcessRecoveryAttemptOutcome::AlreadyApplied::terminal_status [field]
    field_witness(|value: &lash::durability::ProcessRecoveryAttemptOutcome| {
        if let lash::durability::ProcessRecoveryAttemptOutcome::AlreadyApplied {
            terminal_status,
            ..
        } = value
        {
            let _ = terminal_status;
        }
    });
    // W0065: lash::durability::ProcessRecoveryAttemptOutcome::BackendError [variant]
    variant_witness(|value: &lash::durability::ProcessRecoveryAttemptOutcome| {
        matches!(
            value,
            lash::durability::ProcessRecoveryAttemptOutcome::BackendError { .. }
        )
    });
    // W0066: lash::durability::ProcessRecoveryAttemptOutcome::BackendError::error [field]
    field_witness(|value: &lash::durability::ProcessRecoveryAttemptOutcome| {
        if let lash::durability::ProcessRecoveryAttemptOutcome::BackendError { error, .. } = value {
            let _ = error;
        }
    });
    // W0067: lash::durability::ProcessRecoveryAttemptOutcome::BackendError::operation [field]
    field_witness(|value: &lash::durability::ProcessRecoveryAttemptOutcome| {
        if let lash::durability::ProcessRecoveryAttemptOutcome::BackendError { operation, .. } =
            value
        {
            let _ = operation;
        }
    });
    // W0068: lash::durability::ProcessRecoveryAttemptOutcome::Busy [variant]
    variant_witness(|value: &lash::durability::ProcessRecoveryAttemptOutcome| {
        matches!(value, lash::durability::ProcessRecoveryAttemptOutcome::Busy)
    });
    // W0069: lash::durability::ProcessRecoveryAttemptOutcome::LeaseLost [variant]
    variant_witness(|value: &lash::durability::ProcessRecoveryAttemptOutcome| {
        matches!(
            value,
            lash::durability::ProcessRecoveryAttemptOutcome::LeaseLost { .. }
        )
    });
    // W0070: lash::durability::ProcessRecoveryAttemptOutcome::LeaseLost::operation [field]
    field_witness(|value: &lash::durability::ProcessRecoveryAttemptOutcome| {
        if let lash::durability::ProcessRecoveryAttemptOutcome::LeaseLost { operation, .. } = value
        {
            let _ = operation;
        }
    });
    // W0071: lash::durability::ProcessRecoveryAttemptOutcome::SettledByPeer [variant]
    variant_witness(|value: &lash::durability::ProcessRecoveryAttemptOutcome| {
        matches!(
            value,
            lash::durability::ProcessRecoveryAttemptOutcome::SettledByPeer { .. }
        )
    });
    // W0072: lash::durability::ProcessRecoveryAttemptOutcome::SettledByPeer::terminal_status [field]
    field_witness(|value: &lash::durability::ProcessRecoveryAttemptOutcome| {
        if let lash::durability::ProcessRecoveryAttemptOutcome::SettledByPeer {
            terminal_status,
            ..
        } = value
        {
            let _ = terminal_status;
        }
    });
    // W0073: lash::durability::ProcessRecoveryOperation [enum]
    type_witness::<lash::durability::ProcessRecoveryOperation>();
    // W0074: lash::durability::ProcessRecoveryOperation::ClaimLease [variant]
    variant_witness(|value: &lash::durability::ProcessRecoveryOperation| {
        matches!(
            value,
            lash::durability::ProcessRecoveryOperation::ClaimLease
        )
    });
    // W0075: lash::durability::ProcessRecoveryOperation::ReadProcess [variant]
    variant_witness(|value: &lash::durability::ProcessRecoveryOperation| {
        matches!(
            value,
            lash::durability::ProcessRecoveryOperation::ReadProcess
        )
    });
    // W0076: lash::durability::ProcessRecoveryOperation::ReleaseLease [variant]
    variant_witness(|value: &lash::durability::ProcessRecoveryOperation| {
        matches!(
            value,
            lash::durability::ProcessRecoveryOperation::ReleaseLease
        )
    });
    // W0077: lash::durability::ProcessRecoveryOperation::RenewLease [variant]
    variant_witness(|value: &lash::durability::ProcessRecoveryOperation| {
        matches!(
            value,
            lash::durability::ProcessRecoveryOperation::RenewLease
        )
    });
    // W0078: lash::durability::ProcessRecoveryOperation::WriteTerminal [variant]
    variant_witness(|value: &lash::durability::ProcessRecoveryOperation| {
        matches!(
            value,
            lash::durability::ProcessRecoveryOperation::WriteTerminal
        )
    });
    // W0079: lash::durability::RuntimeHostConfig::process_engines [field]
    field_witness(|value: &lash::durability::RuntimeHostConfig| {
        let _ = &value.process_engines;
    });
    // W0081: lash::durability::RuntimeHostConfig::with_process_env_store [function]
    let _ = lash::durability::RuntimeHostConfig::with_process_env_store;
    // W0082: lash::durability::RuntimeHostConfig::with_process_tool_visibility_filter [function]
    let _ = lash::durability::RuntimeHostConfig::with_process_tool_visibility_filter;
    // W0083: lash::durability::RuntimeHostConfig::with_process_wake_delivery_policy [function]
    let _ = lash::durability::RuntimeHostConfig::with_process_wake_delivery_policy;
    // W0084: lash::messages::MessageOrigin::Process [variant]
    variant_witness(|value: &lash::messages::MessageOrigin| {
        matches!(value, lash::messages::MessageOrigin::Process { .. })
    });
    // W0085: lash::messages::MessageOrigin::Process::caused_by [field]
    field_witness(|value: &lash::messages::MessageOrigin| {
        if let lash::messages::MessageOrigin::Process { caused_by, .. } = value {
            let _ = caused_by;
        }
    });
    // W0086: lash::messages::MessageOrigin::Process::event_type [field]
    field_witness(|value: &lash::messages::MessageOrigin| {
        if let lash::messages::MessageOrigin::Process { event_type, .. } = value {
            let _ = event_type;
        }
    });
    // W0087: lash::messages::MessageOrigin::Process::process_id [field]
    field_witness(|value: &lash::messages::MessageOrigin| {
        if let lash::messages::MessageOrigin::Process { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // W0088: lash::messages::MessageOrigin::Process::sequence [field]
    field_witness(|value: &lash::messages::MessageOrigin| {
        if let lash::messages::MessageOrigin::Process { sequence, .. } = value {
            let _ = sequence;
        }
    });
    // W0089: lash::messages::MessageOrigin::Process::wake_id [field]
    field_witness(|value: &lash::messages::MessageOrigin| {
        if let lash::messages::MessageOrigin::Process { wake_id, .. } = value {
            let _ = wake_id;
        }
    });
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
    // W0094: lash::persistence::LeaseOwnerIdentity::restate_process_execution [function]
    let _ = lash::persistence::LeaseOwnerIdentity::restate_process_execution(todo!(), "exec");
    // W0095: lash::persistence::LeaseOwnerIdentity::restate_process_execution_id [function]
    let _ = lash::persistence::LeaseOwnerIdentity::restate_process_execution_id;
    // W0096: lash::persistence::ProcessExecutionEnvStore [trait]
    fn trait_witness_0096<T: lash::persistence::ProcessExecutionEnvStore>() {}
    // W0097: lash::persistence::ProcessExecutionEnvStore::get_process_execution_env [function]
    fn meth_0097<T: lash::persistence::ProcessExecutionEnvStore>(_: &T) {
        let _ = T::get_process_execution_env;
    }
    // W0099: lash::persistence::QueuedWorkBatchDraft::process_wake_source [field]
    field_witness(|value: &lash::persistence::QueuedWorkBatchDraft| {
        let _ = &value.process_wake_source;
    });
    // W0100: lash::persistence::QueuedWorkBatchDraft::with_process_wake_source [function]
    let _: fn(
        lash::persistence::QueuedWorkBatchDraft,
        lash::ProcessId,
        u64,
    ) -> lash::persistence::QueuedWorkBatchDraft =
        lash::persistence::QueuedWorkBatchDraft::with_process_wake_source;
    // W0101: lash::persistence::QueuedWorkPayload::ProcessWake::wake [field]
    field_witness(|value: &lash::persistence::QueuedWorkPayload| {
        if let lash::persistence::QueuedWorkPayload::ProcessWake { wake, .. } = value {
            let _ = wake;
        }
    });
    // W0102: lash::persistence::QueuedWorkPayload::process_wake [function]
    let _ = lash::persistence::QueuedWorkPayload::process_wake;
    // W0103: lash::persistence::StoreError::ProcessWakeSequenceRewound [variant]
    variant_witness(|value: &lash::persistence::StoreError| {
        matches!(
            value,
            lash::persistence::StoreError::ProcessWakeSequenceRewound { .. }
        )
    });
    // W0104: lash::persistence::StoreError::ProcessWakeSequenceRewound::allocation_floor [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::ProcessWakeSequenceRewound {
            allocation_floor, ..
        } = value
        {
            let _ = allocation_floor;
        }
    });
    // W0105: lash::persistence::StoreError::ProcessWakeSequenceRewound::process_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::ProcessWakeSequenceRewound { process_id, .. } = value
        {
            let _ = process_id;
        }
    });
    // W0106: lash::persistence::StoreError::ProcessWakeSequenceRewound::sequence [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::ProcessWakeSequenceRewound { sequence, .. } = value {
            let _ = sequence;
        }
    });
    // W0107: lash::persistence::StoreError::ProcessWakeSequenceRewound::session_id [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::ProcessWakeSequenceRewound { session_id, .. } = value
        {
            let _ = session_id;
        }
    });
    // W0108: lash::plugins::PluginError::ProcessAlreadyStarted [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::ProcessAlreadyStarted { .. }
        )
    });
    // W0109: lash::plugins::PluginError::ProcessAlreadyStarted::by [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessAlreadyStarted { by, .. } = value {
            let _ = by;
        }
    });
    // W0110: lash::plugins::PluginError::ProcessAlreadyStarted::process_id [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessAlreadyStarted { process_id, .. } = value {
            let _ = process_id;
        }
    });
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
    // W0116: lash::plugins::PluginError::ProcessAttemptsExhausted [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::ProcessAttemptsExhausted { .. }
        )
    });
    // W0117: lash::plugins::PluginError::ProcessAttemptsExhausted::attempts [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessAttemptsExhausted { attempts, .. } = value {
            let _ = attempts;
        }
    });
    // W0118: lash::plugins::PluginError::ProcessAttemptsExhausted::max_attempts [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessAttemptsExhausted { max_attempts, .. } = value {
            let _ = max_attempts;
        }
    });
    // W0119: lash::plugins::PluginError::ProcessAttemptsExhausted::process_id [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessAttemptsExhausted { process_id, .. } = value {
            let _ = process_id;
        }
    });
    // W0120: lash::plugins::PluginError::ProcessLeaseSuperseded [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::ProcessLeaseSuperseded { .. }
        )
    });
    // W0121: lash::plugins::PluginError::ProcessLeaseSuperseded::process_id [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessLeaseSuperseded { process_id, .. } = value {
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
    // W0130: lash::plugins::PluginError::ProcessWakeDeliveryFormatVersionMismatch [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::ProcessWakeDeliveryFormatVersionMismatch { .. }
        )
    });
    // W0131: lash::plugins::PluginError::ProcessWakeDeliveryFormatVersionMismatch::expected [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessWakeDeliveryFormatVersionMismatch {
            expected,
            ..
        } = value
        {
            let _ = expected;
        }
    });
    // W0132: lash::plugins::PluginError::ProcessWakeDeliveryFormatVersionMismatch::found [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ProcessWakeDeliveryFormatVersionMismatch {
            found, ..
        } = value
        {
            let _ = found;
        }
    });
    // W0133: lash::plugins::PluginError::ReservedProcessEvent [variant]
    variant_witness(|value: &lash::plugins::PluginError| {
        matches!(
            value,
            lash::plugins::PluginError::ReservedProcessEvent { .. }
        )
    });
    // W0134: lash::plugins::PluginError::ReservedProcessEvent::event_type [field]
    field_witness(|value: &lash::plugins::PluginError| {
        if let lash::plugins::PluginError::ReservedProcessEvent { event_type, .. } = value {
            let _ = event_type;
        }
    });
    // W0135: lash::plugins::PluginHost::install_process_engine_contributions [function]
    let _ = lash::plugins::PluginHost::install_process_engine_contributions::<
        lash::durability::RuntimeHostConfig,
    >(todo!(), todo!(), todo!());
    // W0136: lash::process::AbandonEvidence [struct]
    type_witness::<lash::process::AbandonEvidence>();
    // W0137: lash::process::AbandonRequest [struct]
    type_witness::<lash::process::AbandonRequest>();
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
    // W0157: lash::process::ProcessCompletionAuthority::ExternalOwner [variant]
    variant_witness(|value: &lash::process::ProcessCompletionAuthority| {
        matches!(
            value,
            lash::process::ProcessCompletionAuthority::ExternalOwner
        )
    });
    // W0158: lash::process::ProcessCompletionAuthority::ReconciledAbandon [variant]
    variant_witness(|value: &lash::process::ProcessCompletionAuthority| {
        matches!(
            value,
            lash::process::ProcessCompletionAuthority::ReconciledAbandon
        )
    });
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
    // W0162: lash::process::ProcessContinuationStore [trait]
    fn trait_witness_0162<T: lash::process::ProcessContinuationStore>() {}
    // W0163: lash::process::ProcessContinuationStore::delete_segment_handovers [function]
    fn meth_0163<T: lash::process::ProcessContinuationStore>(_: &T) {
        let _ = T::delete_segment_handovers;
    }
    // W0164: lash::process::ProcessContinuationStore::get_segment_handover [function]
    fn meth_0164<T: lash::process::ProcessContinuationStore>(_: &T) {
        let _ = T::get_segment_handover;
    }
    // W0165: lash::process::ProcessContinuationStore::latest_segment_handover [function]
    fn meth_0165<T: lash::process::ProcessContinuationStore>(_: &T) {
        let _ = T::latest_segment_handover;
    }
    // W0166: lash::process::ProcessContinuationStore::put_segment_handover [function]
    fn meth_0166<T: lash::process::ProcessContinuationStore>(_: &T) {
        let _ = T::put_segment_handover;
    }
    // W0167: lash::process::ProcessEvent [struct]
    type_witness::<lash::process::ProcessEvent>();
    // W0168: lash::process::ProcessEvent::event_type [field]
    field_witness(|value: &lash::process::ProcessEvent| {
        let _ = &value.event_type;
    });
    // W0169: lash::process::ProcessEvent::invocation [field]
    field_witness(|value: &lash::process::ProcessEvent| {
        let _ = &value.invocation;
    });
    // W0170: lash::process::ProcessEvent::occurred_at [field]
    field_witness(|value: &lash::process::ProcessEvent| {
        let _ = &value.occurred_at;
    });
    // W0171: lash::process::ProcessEvent::payload [field]
    field_witness(|value: &lash::process::ProcessEvent| {
        let _ = &value.payload;
    });
    // W0172: lash::process::ProcessEvent::process_id [field]
    field_witness(|value: &lash::process::ProcessEvent| {
        let _ = &value.process_id;
    });
    // W0173: lash::process::ProcessEvent::semantics [field]
    field_witness(|value: &lash::process::ProcessEvent| {
        let _ = &value.semantics;
    });
    // W0174: lash::process::ProcessEvent::sequence [field]
    field_witness(|value: &lash::process::ProcessEvent| {
        let _ = &value.sequence;
    });
    // W0175: lash::process::ProcessEventAppendRequest::abandon_requested [function]
    let _ = lash::process::ProcessEventAppendRequest::abandon_requested;
    // W0176: lash::process::ProcessEventAppendRequest::cancel_requested [function]
    let _ = lash::process::ProcessEventAppendRequest::cancel_requested;
    // W0177: lash::process::ProcessEventAppendRequest::event_type [field]
    field_witness(|value: &lash::process::ProcessEventAppendRequest| {
        let _ = &value.event_type;
    });
    // W0178: lash::process::ProcessEventAppendRequest::external_ref_set [function]
    let _ = lash::process::ProcessEventAppendRequest::external_ref_set;
    // W0179: lash::process::ProcessEventAppendRequest::first_started [function]
    let _ = lash::process::ProcessEventAppendRequest::first_started;
    // W0180: lash::process::ProcessEventAppendRequest::observer_added [function]
    let _ = lash::process::ProcessEventAppendRequest::observer_added;
    // W0181: lash::process::ProcessEventAppendRequest::observer_removed [function]
    let _ = lash::process::ProcessEventAppendRequest::observer_removed;
    // W0182: lash::process::ProcessEventAppendRequest::payload [field]
    field_witness(|value: &lash::process::ProcessEventAppendRequest| {
        let _ = &value.payload;
    });
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
    // W0188: lash::process::ProcessEventAppendReceipt::wake_delivery [field]
    field_witness(|value: &lash::process::ProcessEventAppendReceipt| {
        let _ = &value.wake_delivery;
    });
    // W0189: lash::process::ProcessEventSemanticsSpec::terminal [field]
    field_witness(|value: &lash::process::ProcessEventSemanticsSpec| {
        let _ = &value.terminal;
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
    // W0197: lash::process::ProcessExecutionEnvSpec::plugin_options [field]
    field_witness(|value: &lash::process::ProcessExecutionEnvSpec| {
        let _ = &value.plugin_options;
    });
    // W0198: lash::process::ProcessExecutionEnvSpec::policy [field]
    field_witness(|value: &lash::process::ProcessExecutionEnvSpec| {
        let _ = &value.policy;
    });
    // W0199: lash::process::ProcessExecutionEnvSpec::to_store_bytes [function]
    let _ = lash::process::ProcessExecutionEnvSpec::to_store_bytes;
    // W0200: lash::process::ProcessHandleView::new [function]
    let _ = lash::process::ProcessHandleView::new("x", todo!(), todo!(), todo!());
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
    // W0207: lash::process::ProcessInput::SessionTurn::output_contract [field]
    field_witness(|value: &lash::process::ProcessInput| {
        if let lash::process::ProcessInput::SessionTurn {
            output_contract, ..
        } = value
        {
            let _ = output_contract;
        }
    });
    // W0208: lash::process::ProcessInput::SessionTurn::turn_input [field]
    field_witness(|value: &lash::process::ProcessInput| {
        if let lash::process::ProcessInput::SessionTurn { turn_input, .. } = value {
            let _ = turn_input;
        }
    });
    // W0209: lash::process::ProcessInput::ToolCall [variant]
    variant_witness(|value: &lash::process::ProcessInput| {
        matches!(value, lash::process::ProcessInput::ToolCall { .. })
    });
    // W0210: lash::process::ProcessInput::ToolCall::call [field]
    field_witness(|value: &lash::process::ProcessInput| {
        if let lash::process::ProcessInput::ToolCall { call, .. } = value {
            let _ = call;
        }
    });
    // W0211: lash::process::ProcessLeaseClaimOutcome::Busy::holder [field]
    field_witness(|value: &lash::process::ProcessLeaseClaimOutcome| {
        if let lash::process::ProcessLeaseClaimOutcome::Busy { holder, .. } = value {
            let _ = holder;
        }
    });
    // W0212: lash::process::ProcessLeaseClaimOutcome::acquired [function]
    let _ = lash::process::ProcessLeaseClaimOutcome::acquired;
    // W0213: lash::process::ProcessLeaseCompletion [struct]
    type_witness::<lash::process::ProcessLeaseCompletion>();
    // W0214: lash::process::ProcessLeaseCompletion::from_lease [function]
    let _ = lash::process::ProcessLeaseCompletion::from_lease;
    // W0215: lash::process::ProcessLeaseCompletion::lease_token [field]
    field_witness(|value: &lash::process::ProcessLeaseCompletion| {
        let _ = &value.lease_token;
    });
    // W0216: lash::process::ProcessLeaseCompletion::process_id [field]
    field_witness(|value: &lash::process::ProcessLeaseCompletion| {
        let _ = &value.process_id;
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
    // W0235: lash::process::ProcessRegistration::with_event_types [function]
    let _ = lash::process::ProcessRegistration::with_event_types(
        todo!(),
        std::iter::empty::<lash::process::ProcessEventType>(),
    );
    // W0236: lash::process::ProcessRegistry::add_observer [function]
    fn meth_0236<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::add_observer;
    }
    // W0237: lash::process::ProcessRegistry::append_event_with_authority [function]
    fn meth_0237<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::append_event_with_authority;
    }
    // W0238: lash::process::ProcessRegistry::claim_pending_wake_deliveries [function]
    fn meth_0238<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::claim_pending_wake_deliveries;
    }
    // W0239: lash::process::ProcessRegistry::clear_process_wait_with_authority [function]
    fn meth_0239<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::clear_process_wait_with_authority;
    }
    // W0240: lash::process::ProcessRegistry::complete_process_lease [function]
    fn meth_0240<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::complete_process_lease;
    }
    // W0241: lash::process::ProcessRegistry::defer_wake_delivery [function]
    fn meth_0241<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::defer_wake_delivery;
    }
    // W0242: lash::process::ProcessRegistry::delete_session_process_state [function]
    fn meth_0242<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::delete_session_process_state;
    }
    // W0243: lash::process::ProcessRegistry::discard_wake_delivery [function]
    fn meth_0243<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::discard_wake_delivery;
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
    // W0248: lash::process::ProcessRegistry::list_wake_deliveries [function]
    fn meth_0248<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::list_wake_deliveries;
    }
    // W0249: lash::process::ProcessRegistry::mark_wake_enqueued [function]
    fn meth_0249<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::mark_wake_enqueued;
    }
    // W0250: lash::process::ProcessRegistry::processes_changed_since [function]
    fn meth_0250<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::processes_changed_since;
    }
    // W0251: lash::process::ProcessRegistry::reclaim_process_lease [function]
    fn meth_0251<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::reclaim_process_lease;
    }
    // W0252: lash::process::ProcessRegistry::record_first_started_with_authority [function]
    fn meth_0252<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::record_first_started_with_authority;
    }
    // W0253: lash::process::ProcessRegistry::redrive_wake_delivery [function]
    fn meth_0253<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::redrive_wake_delivery;
    }
    // W0254: lash::process::ProcessRegistry::request_process_abandon [function]
    fn meth_0254<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::request_process_abandon;
    }
    // W0255: lash::process::ProcessRegistry::set_process_wait_with_authority [function]
    fn meth_0255<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::set_process_wait_with_authority;
    }
    // W0256: lash::process::ProcessRegistry::wake_delivery_config [function]
    fn meth_0256<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::wake_delivery_config;
    }
    // W0257: lash::process::ProcessRegistry::wake_delivery_report [function]
    fn meth_0257<T: lash::process::ProcessRegistry>(_: &T) {
        let _ = T::wake_delivery_report;
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
    // W0264: lash::process::ProcessService::complete_external [function]
    fn meth_0264<T: lash::process::ProcessService>(_: &T) {
        let _ = T::complete_external;
    }
    // W0265: lash::process::ProcessService::list_visible [function]
    fn meth_0265<T: lash::process::ProcessService>(_: &T) {
        let _ = T::list_visible;
    }
    // W0266: lash::process::ProcessService::emit_event_recorded_intent [function]
    fn meth_0266<T: lash::process::ProcessService>(_: &T) {
        let _ = T::emit_event_recorded_intent;
    }
    // W0267: lash::process::ProcessService::signal_recorded_intent [function]
    fn meth_0267<T: lash::process::ProcessService>(_: &T) {
        let _ = T::signal_recorded_intent;
    }
    // W0268: lash::process::ProcessService::signal_possessed [function]
    fn meth_0268<T: lash::process::ProcessService>(_: &T) {
        let _ = T::signal_possessed;
    }
    // W0269: lash::process::ProcessService::start [function]
    fn meth_0269<T: lash::process::ProcessService>(_: &T) {
        let _ = T::start;
    }
    // W0270: lash::process::ProcessService::start_from_recorded_intent [function]
    fn meth_0270<T: lash::process::ProcessService>(_: &T) {
        let _ = T::start_from_recorded_intent;
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
    // W0275: lash::process::ProcessSessionDeleteReport::discarded_wake_delivery_count [field]
    field_witness(|value: &lash::process::ProcessSessionDeleteReport| {
        let _ = &value.discarded_wake_delivery_count;
    });
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
    // W0287: lash::process::ProcessStartRequest::disposition [field]
    field_witness(|value: &lash::process::ProcessStartRequest| {
        let _ = &value.disposition;
    });
    // W0288: lash::process::ProcessStartRequest::env_spec [field]
    field_witness(|value: &lash::process::ProcessStartRequest| {
        let _ = &value.env_spec;
    });
    // W0289: lash::process::ProcessStartRequest::event_types [field]
    field_witness(|value: &lash::process::ProcessStartRequest| {
        let _ = &value.event_types;
    });
    // W0290: lash::process::ProcessStartRequest::external [function]
    let _ = lash::process::ProcessStartRequest::external("x", todo!(), todo!(), todo!());
    // W0291: lash::process::ProcessStartRequest::id [field]
    field_witness(|value: &lash::process::ProcessStartRequest| {
        let _ = &value.id;
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
    // W0295: lash::process::ProcessStartRequest::max_attempts [field]
    field_witness(|value: &lash::process::ProcessStartRequest| {
        let _ = &value.max_attempts;
    });
    // W0296: lash::process::ProcessStartRequest::new [function]
    let _ = lash::process::ProcessStartRequest::new("x", todo!(), todo!(), todo!(), todo!());
    // W0297: lash::process::ProcessStartRequest::observers [field]
    field_witness(|value: &lash::process::ProcessStartRequest| {
        let _ = &value.observers;
    });
    // W0298: lash::process::ProcessStartRequest::originator [field]
    field_witness(|value: &lash::process::ProcessStartRequest| {
        let _ = &value.originator;
    });
    // W0299: lash::process::ProcessStartRequest::wake_session_id [field]
    field_witness(|value: &lash::process::ProcessStartRequest| {
        let _ = &value.wake_session_id;
    });
    // W0300: lash::process::ProcessStartRequest::with_env_spec [function]
    let _ = lash::process::ProcessStartRequest::with_env_spec;
    // W0301: lash::process::ProcessStartRequest::with_event_types [function]
    let _ = lash::process::ProcessStartRequest::with_event_types(
        todo!(),
        std::iter::empty::<lash::process::ProcessEventType>(),
    );
    // W0302: lash::process::ProcessStartRequest::with_extra_event_types [function]
    let _ = lash::process::ProcessStartRequest::with_extra_event_types(
        todo!(),
        std::iter::empty::<lash::process::ProcessEventType>(),
    );
    // W0304: lash::process::ProcessStartRequest::with_max_attempts [function]
    let _ = lash::process::ProcessStartRequest::with_max_attempts;
    // W0305: lash::process::ProcessStartRequest::with_observers [function]
    let _ = lash::process::ProcessStartRequest::with_observers(
        todo!(),
        std::iter::empty::<lash::SessionId>(),
    );
    // W0306: lash::process::ProcessStartRequest::with_wake_session_id [function]
    let _ = lash::process::ProcessStartRequest::with_wake_session_id;
    // W0307: lash::process::ProcessStatus::Abandoned [variant]
    variant_witness(|value: &lash::process::ProcessStatus| {
        matches!(value, lash::process::ProcessStatus::Abandoned)
    });
    // W0308: lash::process::ProcessStatus::Waiting [variant]
    variant_witness(|value: &lash::process::ProcessStatus| {
        matches!(value, lash::process::ProcessStatus::Waiting)
    });
    // W0314: lash::process::ProcessValueSelector::Const [variant]
    variant_witness(|value: &lash::process::ProcessValueSelector| {
        matches!(value, lash::process::ProcessValueSelector::Const(..))
    });
    // W0315: lash::process::ProcessValueSelector::Const::0 [field]
    field_witness(|value: &lash::process::ProcessValueSelector| {
        if let lash::process::ProcessValueSelector::Const(f0) = value {
            let _ = f0;
        }
    });
    // W0316: lash::process::ProcessValueSelector::Payload [variant]
    variant_witness(|value: &lash::process::ProcessValueSelector| {
        matches!(value, lash::process::ProcessValueSelector::Payload)
    });
    // W0317: lash::process::ProcessValueSelector::Template [variant]
    variant_witness(|value: &lash::process::ProcessValueSelector| {
        matches!(value, lash::process::ProcessValueSelector::Template { .. })
    });
    // W0318: lash::process::ProcessValueSelector::Template::fields [field]
    field_witness(|value: &lash::process::ProcessValueSelector| {
        if let lash::process::ProcessValueSelector::Template { fields, .. } = value {
            let _ = fields;
        }
    });
    // W0319: lash::process::ProcessValueSelector::Template::template [field]
    field_witness(|value: &lash::process::ProcessValueSelector| {
        if let lash::process::ProcessValueSelector::Template { template, .. } = value {
            let _ = template;
        }
    });
    // W0320: lash::process::ProcessWake [struct]
    type_witness::<lash::process::ProcessWake>();
    // W0321: lash::process::ProcessWake::input [field]
    field_witness(|value: &lash::process::ProcessWake| {
        let _ = &value.input;
    });
    // W0322: lash::process::ProcessWakeDelivery [struct]
    type_witness::<lash::process::ProcessWakeDelivery>();
    // W0323: lash::process::ProcessWakeDelivery::authority [field]
    field_witness(|value: &lash::process::ProcessWakeDelivery| {
        let _ = &value.authority;
    });
    // W0324: lash::process::ProcessWakeDelivery::created_at_ms [field]
    field_witness(|value: &lash::process::ProcessWakeDelivery| {
        let _ = &value.created_at_ms;
    });
    // W0325: lash::process::ProcessWakeDelivery::event_invocation [field]
    field_witness(|value: &lash::process::ProcessWakeDelivery| {
        let _ = &value.event_invocation;
    });
}
