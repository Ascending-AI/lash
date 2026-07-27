use std::sync::Arc;

use lash_core::{
    LeaseOwnerIdentity, LeaseOwnerLiveness, ProcessAwaitOutput, ProcessRegistry,
    RecoveryDisposition,
};
use serde_json::{Value, json};

use super::RuntimeBoundaryError;

pub(super) fn local_process_owner(
    host: &str,
    boot: &str,
    owner_id: &str,
    process_start: &str,
) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity {
        owner_id: owner_id.to_string(),
        incarnation_id: format!("{owner_id}:incarnation"),
        liveness: LeaseOwnerLiveness::local_process_for_test(
            host,
            boot,
            std::process::id(),
            process_start,
        ),
    }
}

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
    ) -> lash_core::ProcessRunOutcome {
        ProcessAwaitOutput::Success {
            value: json!({"recovered": true}),
            control: None,
        }
        .into()
    }
}

/// Recovery worker with a real producer engine for the Rerunnable row. This
/// keeps a successful producer outcome distinct from infrastructure failure,
/// which intentionally writes no terminal.
pub(super) fn lifecycle_worker(
    registry: Arc<dyn ProcessRegistry>,
    owner: LeaseOwnerIdentity,
    runtime_host: lash_core::RuntimeHostConfig,
    policy: lash_core::SessionPolicy,
) -> lash_core::DurableProcessWorker {
    lash_core::DurableProcessWorker::new(
        lash_core::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::PluginHost::new(vec![Arc::new(
                lash_protocol_standard::StandardProtocolPluginFactory::new(),
            )])),
            runtime_host,
            Arc::new(lash_core::InMemorySessionStoreFactory::new()),
            registry,
        )
        .with_session_policy(policy)
        .with_lease_owner(owner),
    )
}

pub(super) async fn register_lifecycle_row(
    registry: &dyn ProcessRegistry,
    id: &str,
    disposition: RecoveryDisposition,
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
                RecoveryDisposition::Rerunnable,
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
/// liveness check and a registry read), so the evidence oracle cross-checks the
/// writer against ground truth rather than trusting it.
pub(super) async fn lifecycle_process_fact(
    registry: &Arc<dyn ProcessRegistry>,
    awaiter: &lash_core::ProcessAwaiter,
    id: &str,
    disposition: RecoveryDisposition,
    expected_holder: Option<&LeaseOwnerIdentity>,
    sweep_owner: &LeaseOwnerIdentity,
) -> Result<Value, RuntimeBoundaryError> {
    let output = match tokio::time::timeout(
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
    };
    let record = registry.get_process(id).await.ok_or_else(|| {
        RuntimeBoundaryError::new(format!("process `{id}` vanished after terminal"))
    })?;
    let reran = matches!(
        record.status,
        lash_core::ProcessStatus::Completed { .. }
            | lash_core::ProcessStatus::Failed { .. }
            | lash_core::ProcessStatus::Cancelled { .. }
    );
    let provably_dead_holder =
        expected_holder.is_some_and(|holder| holder.is_definitely_dead_for_claimant(sweep_owner));
    let lease_lapsed = registry
        .get_process_lease(id)
        .await
        .map_err(|err| RuntimeBoundaryError::new(format!("read lease for `{id}` failed: {err}")))?
        .is_none();
    let first_started_owner = record.first_started.as_ref().map(|started| {
        if started.owner.owner_id == sweep_owner.owner_id
            || started
                .owner
                .owner_id
                .starts_with(&format!("{}:", sweep_owner.owner_id))
        {
            sweep_owner.owner_id.clone()
        } else {
            started.owner.owner_id.clone()
        }
    });
    let mut fact = json!({
        "process_id": id,
        "disposition": disposition_str(disposition),
        "started": record.first_started.is_some(),
        "terminal_status": record.status.label(),
        "reran": reran,
        "provably_dead_holder": provably_dead_holder,
        "lease_lapsed": lease_lapsed,
        "abandon_requested": record.abandon_request.is_some(),
        "first_started_owner": first_started_owner,
    });
    if let ProcessAwaitOutput::Abandoned { evidence, .. } = &output {
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

fn disposition_str(disposition: RecoveryDisposition) -> &'static str {
    match disposition {
        RecoveryDisposition::Rerunnable => "rerunnable",
        RecoveryDisposition::OwnerBound => "owner_bound",
        RecoveryDisposition::ExternallyOwned => "externally_owned",
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
