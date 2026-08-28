use std::sync::Arc;

use lash_core::sync::MutexExt as _;
use lash_core::{
    LeaseOwnerIdentity, ProcessAwaitOutput, ProcessRegistry, RecoveryContract,
    TestProcessRegistryWriteExt,
};
use serde_json::{Value, json};

use super::RuntimeBoundaryError;

pub(super) struct LifecycleSuccessEngine;

#[async_trait::async_trait]
impl lash_core::ProcessEngine for LifecycleSuccessEngine {
    fn kind(&self) -> &'static str {
        "sim-lifecycle"
    }

    async fn run(
        &self,
        _context: lash_core::ProcessEngineRunContext<'_>,
        _payload: Value,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
        Ok(
            ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                json!({"recovered": true}),
            ))
            .into(),
        )
    }
}

/// Records every worker fault the sweep reports.
///
/// The sweep is admission-only: a claim, read, terminal write, or lease release
/// that fails after the drive returns has no other way into the scenario. The
/// boundary asserts this list is empty, so a swallowed fault cannot let the
/// recorded verdicts pass vacuously.
#[derive(Clone, Default)]
pub(super) struct RecordingWorkerFaultSink {
    faults: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl lash_core::facade_support::ProcessEventSink for RecordingWorkerFaultSink {
    async fn emit(&self, _event: &lash_core::ProcessEvent) {}

    async fn emit_worker_fault(&self, fault: &lash_core::facade_support::ProcessWorkerFault) {
        self.faults.lock_recover().push(format!("{fault:?}"));
    }
}

impl RecordingWorkerFaultSink {
    pub(super) fn recorded(&self) -> Vec<String> {
        self.faults.lock_recover().clone()
    }
}

/// Recovery worker with a real producer engine for the Rerunnable row. This
/// keeps a successful producer outcome distinct from infrastructure failure,
/// which intentionally writes no terminal.
pub(super) fn lifecycle_worker(
    registry: Arc<dyn ProcessRegistry>,
    owner: LeaseOwnerIdentity,
    runtime_host: lash_core::facade_support::RuntimeHostConfig,
    policy: lash_core::SessionPolicy,
    fault_sink: &RecordingWorkerFaultSink,
) -> lash_core::facade_support::DurableProcessWorker {
    let watched = lash_core::facade_support::watch_process_registry(registry);
    lash_core::facade_support::DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(vec![Arc::new(
                lash_protocol_standard::StandardProtocolPluginFactory::new(),
            )])),
            runtime_host,
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            lash_core::WorkerProcessWork::SelfNative(watched),
            Arc::new(lash_core::NoQueuedWork::new()),
            owner,
        )
        .with_session_policy(policy)
        .with_process_event_sink(Arc::new(fault_sink.clone())),
    )
    .expect("simulation worker uses valid native substrate defaults")
}

pub(super) async fn register_lifecycle_row(
    registry: &dyn ProcessRegistry,
    id: &str,
    disposition: RecoveryContract,
) -> Result<(), RuntimeBoundaryError> {
    registry
        .register_process(lash_core::ProcessRegistration::new(
            id,
            lash_core::ProcessInput::External {
                metadata: json!({}),
            },
            disposition,
            lash_core::ProcessProvenance::host(),
        ))
        .await
        .map(|_| ())
        .map_err(|err| RuntimeBoundaryError::new(format!("register `{id}` failed: {err}")))
}

pub(super) async fn register_rerunnable_lifecycle_row(
    registry: &dyn ProcessRegistry,
    id: &str,
    env_ref: lash_core::ProcessExecutionEnvRef,
) -> Result<(), RuntimeBoundaryError> {
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                id,
                lash_core::ProcessInput::Engine {
                    kind: "sim-lifecycle".to_string(),
                    payload: json!({}),
                },
                RecoveryContract::Rerunnable,
                lash_core::ProcessProvenance::host(),
            )
            .with_execution_env_ref(Some(env_ref)),
        )
        .await
        .map(|_| ())
        .map_err(|err| RuntimeBoundaryError::new(format!("register `{id}` failed: {err}")))
}

pub(super) async fn record_lifecycle_started(
    registry: &dyn ProcessRegistry,
    id: &str,
    owner: &LeaseOwnerIdentity,
) -> Result<(), RuntimeBoundaryError> {
    registry
        .record_first_started(
            id,
            lash_core::ProcessStarted {
                owner: owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await
        .map(|_| ())
        .map_err(|err| {
            RuntimeBoundaryError::new(format!("record first_started for `{id}` failed: {err}"))
        })
}

/// Await a swept row's terminal and record its verdict facts. Death and
/// authorization are observed INDEPENDENTLY of the abandon writer (a real
/// lease and registry reads), so the evidence oracle cross-checks the writer
/// against ground truth rather than trusting it.
pub(super) async fn lifecycle_process_fact(
    registry: &Arc<dyn ProcessRegistry>,
    awaiter: &lash_core::NativeProcessWork,
    id: &str,
    disposition: RecoveryContract,
    expected_holder: Option<&LeaseOwnerIdentity>,
    _sweep_owner: &LeaseOwnerIdentity,
) -> Result<Value, RuntimeBoundaryError> {
    let should_remain_non_terminal =
        disposition == RecoveryContract::OwnerBound && expected_holder.is_some();
    let output = if should_remain_non_terminal {
        None
    } else {
        Some(
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                awaiter.await_terminal(id),
            )
            .await
            {
                Ok(result) => result.map_err(|err| {
                    RuntimeBoundaryError::new(format!("await terminal for `{id}` failed: {err}"))
                })?,
                Err(_) => {
                    let record = registry.get_process(id).await;
                    let lease = registry.get_process_lease(id).await.map_err(|err| {
                        RuntimeBoundaryError::new(format!(
                            "read timed-out lifecycle lease for `{id}` failed: {err}"
                        ))
                    })?;
                    return Err(RuntimeBoundaryError::new(format!(
                        "timed out awaiting lifecycle terminal for `{id}`: record={record:?}, lease={lease:?}"
                    )));
                }
            },
        )
    };
    let record = registry
        .get_process(id)
        .await
        .map_err(|err| RuntimeBoundaryError::new(format!("read process `{id}`: {err}")))?
        .ok_or_else(|| {
            RuntimeBoundaryError::new(format!("process `{id}` vanished after terminal"))
        })?;
    let reran = matches!(
        record.status,
        lash_core::ProcessStatus::Completed
            | lash_core::ProcessStatus::Failed
            | lash_core::ProcessStatus::Cancelled
    );
    let lease_lapsed = registry
        .get_process_lease(id)
        .await
        .map_err(|err| RuntimeBoundaryError::new(format!("read lease for `{id}` failed: {err}")))?
        .is_none();
    let mut fact = json!({
        "process_id": id,
        "disposition": disposition_str(disposition),
        "started": record.first_started.is_some(),
        "terminal_status": record.status.label(),
        "reran": reran,
        "lease_lapsed": lease_lapsed,
        "abandon_requested": record.abandon_request.is_some(),
        "first_started_owner": record
            .first_started
            .as_ref()
            .map(|started| started.owner.owner_id.clone()),
    });
    if let Some(ProcessAwaitOutput::Abandoned { evidence, .. }) = &output {
        let obj = fact.as_object_mut().expect("lifecycle fact is an object");
        obj.insert(
            "abandon_writer".to_string(),
            json!(abandon_writer_str(evidence.writer)),
        );
        obj.insert(
            "abandon_evidence_owner".to_string(),
            json!(evidence.owner.as_ref().map(|owner| owner.owner_id.clone())),
        );
    }
    Ok(fact)
}

fn disposition_str(disposition: RecoveryContract) -> &'static str {
    match disposition {
        RecoveryContract::Rerunnable => "rerunnable",
        RecoveryContract::OwnerBound => "owner_bound",
        RecoveryContract::ExternallyOwned => "externally_owned",
    }
}

fn abandon_writer_str(writer: lash_core::AbandonWriter) -> &'static str {
    match writer {
        lash_core::AbandonWriter::OwnerDrain => "owner_drain",
        lash_core::AbandonWriter::Sweep => "sweep",
        lash_core::AbandonWriter::ReconciledRequest => "reconciled_request",
        lash_core::AbandonWriter::EngineGaveUp => "engine_gave_up",
    }
}
