//! Parent-held bytes and read-only guest projections. All state operations
//! which open a snapshot run in a worker through the service.
use crate::service::runtime_ops::ServiceRuntimeOps as _;
use lash_core_execution::FleetFormat;
use lash_vm::{Record, Value};
use std::collections::BTreeSet;

use crate::service::{
    Capture, CellCompletion, Request, Response, Service, StateAction, StateMetadata, StateView,
};

#[derive(Clone)]
pub struct RemoteState {
    service: Service,
    view: StateView,
}
impl RemoteState {
    pub fn pristine(service: Service) -> Self {
        Self {
            service,
            view: StateView {
                snapshot: Vec::new(),
                metadata: Default::default(),
            },
        }
    }
    pub fn service(&self) -> &Service {
        &self.service
    }
    pub fn replace_service(&mut self, service: Service) -> Service {
        std::mem::replace(&mut self.service, service)
    }
    pub fn bytes(&self) -> Option<&[u8]> {
        (!self.view.snapshot.is_empty()).then_some(&self.view.snapshot)
    }
    pub fn globals(&self) -> &Record {
        &self.view.metadata.globals
    }
    pub fn binding_names(&self) -> impl Iterator<Item = &str> {
        self.view.metadata.names.iter().map(String::as_str)
    }
    pub fn expired_functions(&self) -> &BTreeSet<String> {
        &self.view.metadata.expired
    }
    pub fn opaque_bindings(&self) -> Vec<(String, String)> {
        self.view.metadata.opaque.clone()
    }
    /// Render opaque guest values in the worker with the host's presentation policy.
    pub async fn opaque_bindings_with(
        &self,
        config: &lash_vm::BindingSummaryConfig,
    ) -> Result<Vec<(String, String)>, crate::PoolError> {
        let Some(snapshot) = self.bytes() else {
            return Ok(Vec::new());
        };
        if self.view.metadata.opaque.is_empty() {
            return Ok(Vec::new());
        }
        match self
            .service
            .request_accounted(Request::OpaqueBindings {
                snapshot: snapshot.to_vec().into(),
                config: *config,
            })
            .await?
        {
            Response::OpaqueBindings(bindings) => Ok(bindings),
            _ => Err(crate::PoolError::breach(
                lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
            )),
        }
    }
    pub fn referenced_definition_ids(&self) -> BTreeSet<lash_core_execution::ProcessDefinitionId> {
        self.view.metadata.definition_ids.clone()
    }
    /// Adopt a broker-checked completion without reopening its VM snapshot.
    pub fn install_completion(
        &mut self,
        snapshot: &lash_vm_protocol::OpaqueVmState,
        value: &lash_vm_protocol::EncodedPayload,
    ) -> Result<lash_vm::ExecutionOutcome, crate::PoolError> {
        let codec = lash_vm_protocol::FrameCodec::new(self.service.config().protocol.decode);
        codec.check_payload(&value.0)?;
        let completion: CellCompletion = rmp_serde::from_slice(&value.0).map_err(|error| {
            crate::PoolError::payload(lash_vm_protocol::PayloadKind::Completion, error)
        })?;
        codec.check_payload(&completion.state.0)?;
        let metadata: StateMetadata =
            rmp_serde::from_slice(&completion.state.0).map_err(|error| {
                crate::PoolError::payload(lash_vm_protocol::PayloadKind::Completion, error)
            })?;
        if !metadata
            .definition_ids
            .iter()
            .eq(snapshot.definition_ids().iter())
        {
            return Err(crate::PoolError::breach(
                lash_vm_protocol::SequenceFault::CompletionDefinitionsMismatch,
            ));
        }
        self.view = StateView {
            snapshot: snapshot.bytes().to_vec(),
            metadata,
        };
        Ok(completion.outcome)
    }
    pub async fn install_bytes(&mut self, bytes: Vec<u8>) -> Result<(), crate::PoolError> {
        match self
            .service
            .request_accounted(Request::State {
                snapshot: Some(bytes.into()),
                action: StateAction::Inspect,
            })
            .await?
        {
            Response::State(view) => {
                self.view = view;
                Ok(())
            }
            _ => Err(crate::PoolError::breach(
                lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
            )),
        }
    }
    pub async fn insert_global(
        &mut self,
        name: impl Into<String>,
        value: Value,
    ) -> Result<(), crate::PoolError> {
        self.mutate(StateAction::Insert {
            name: name.into(),
            value,
        })
        .await
    }
    pub async fn remove_global(&mut self, name: &str) -> Result<bool, crate::PoolError> {
        if !self.view.metadata.names.contains(name) {
            return Ok(false);
        }
        self.mutate(StateAction::Remove {
            names: BTreeSet::from([name.to_string()]),
        })
        .await?;
        Ok(true)
    }
    pub async fn remove_names(&mut self, names: BTreeSet<String>) -> Result<(), crate::PoolError> {
        self.mutate(StateAction::Remove { names }).await
    }
    pub async fn defaults(
        &mut self,
        values: std::collections::BTreeMap<String, Value>,
        protected: BTreeSet<String>,
    ) -> Result<(), crate::PoolError> {
        self.mutate(StateAction::Defaults { values, protected })
            .await
    }
    async fn mutate(&mut self, action: StateAction) -> Result<(), crate::PoolError> {
        match self
            .service
            .request_accounted(Request::State {
                snapshot: self.bytes().map(serde_bytes::ByteBuf::from),
                action,
            })
            .await?
        {
            Response::State(view) => {
                self.view = view;
                Ok(())
            }
            _ => Err(crate::PoolError::breach(
                lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
            )),
        }
    }
    pub async fn capture(
        &self,
        baseline: &std::collections::BTreeMap<String, String>,
        fleet: FleetFormat,
    ) -> Result<Capture, crate::PoolError> {
        let snapshot = match self.bytes() {
            Some(bytes) => bytes.to_vec(),
            None => match self
                .service
                .request_accounted(Request::State {
                    snapshot: None,
                    action: StateAction::Inspect,
                })
                .await?
            {
                Response::State(view) => view.snapshot,
                _ => {
                    return Err(crate::PoolError::breach(
                        lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
                    ));
                }
            },
        };
        match self
            .service
            .request_accounted(Request::Capture {
                snapshot: snapshot.into(),
                baseline: baseline.clone(),
                fleet: fleet.version(),
            })
            .await?
        {
            Response::Captured(parts) => Ok(parts),
            _ => Err(crate::PoolError::breach(
                lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
            )),
        }
    }
    pub async fn restore(
        &mut self,
        header: Vec<u8>,
        globals: std::collections::BTreeMap<String, Vec<u8>>,
        fleet: FleetFormat,
    ) -> Result<std::collections::BTreeMap<String, String>, RemoteRestoreError> {
        match self
            .service
            .request_accounted(Request::Restore {
                header: header.into(),
                globals: globals.into_iter().map(|(k, v)| (k, v.into())).collect(),
                fleet: fleet.version(),
            })
            .await
            .map_err(RemoteRestoreError::Worker)?
        {
            Response::Restored { view, baseline } => {
                self.view = view;
                Ok(baseline)
            }
            Response::SnapshotRefused(error) => Err(RemoteRestoreError::Snapshot(error)),
            _ => Err(RemoteRestoreError::Worker(crate::PoolError::breach(
                lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
            ))),
        }
    }
}
#[derive(Clone)]
pub struct RemoteVm {
    state: RemoteState,
}
impl RemoteVm {
    pub fn pristine(service: Service) -> Self {
        Self {
            state: RemoteState::pristine(service),
        }
    }
    pub fn state(&self) -> &RemoteState {
        &self.state
    }
    pub fn state_mut(&mut self) -> &mut RemoteState {
        &mut self.state
    }
    pub fn replace_state(&mut self, state: RemoteState) -> RemoteState {
        std::mem::replace(&mut self.state, state)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteRestoreError {
    #[error(transparent)]
    Snapshot(lash_vm::SnapshotDecodeError),
    #[error(transparent)]
    Worker(crate::PoolError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::StateMetadata;
    use lash_vm_protocol::{
        EncodedPayload, InfrastructureOutcome, OpaqueVmState, VmOwner, VmStateKind,
    };

    #[tokio::test(flavor = "current_thread")]
    async fn saturated_state_request_does_not_delay_an_unrelated_timer() {
        use crate::WorkerPoolRuntimeOps as _;
        use std::time::{Duration, Instant};

        let mut config = crate::PoolConfig::standard(crate::WorkerEntry::helper(
            crate::testing::worker_executable("lash-vm-worker".into()),
        ));
        config.max_workers = 1;
        config.deadlines.checkout = Duration::from_secs(1);
        let service = Service::new(config);
        let pool = service
            .pool()
            .expect("prewarm outside the measured interval");
        let held = pool
            .checkout(
                4096,
                crate::OwnerEpoch(0),
                crate::FrameEpoch(0),
                Default::default(),
            )
            .expect("hold the only worker");
        let mut state = RemoteState::pristine(service);
        let (elapsed, result) = tokio::join!(
            biased;
            async {
                let started = Instant::now();
                tokio::time::sleep(Duration::from_millis(20)).await;
                started.elapsed()
            },
            async { state.insert_global("value", Value::Number(42.0)).await },
        );
        drop(held);
        assert!(
            matches!(result, Err(crate::PoolError::CheckoutTimedOut)),
            "{result:?}"
        );
        let runtime = result.expect_err("saturated checkout").into_runtime_error();
        let plugin = lash_core_execution::PluginError::Runtime(runtime);
        let encoded = rmp_serde::to_vec_named(&plugin).expect("plugin failure encodes");
        let plugin: lash_core_execution::PluginError =
            rmp_serde::from_slice(&encoded).expect("plugin failure decodes");
        let runtime = plugin.into_turn_failure(lash_core_execution::RuntimeErrorCode::Plugin);
        assert_eq!(
            runtime.code,
            lash_core_execution::RuntimeErrorCode::WorkerCheckoutTimedOut
        );
        assert!(
            matches!(runtime.cause, Some(lash_core_execution::RuntimeErrorCause::VmWorker { ref outcome }) if **outcome == crate::PoolError::CheckoutTimedOut.into_outcome())
        );
        assert!(!runtime.is_terminal());
        eprintln!("saturated checkout left the 20 ms timer responsive after {elapsed:?}");
        assert!(
            elapsed < Duration::from_millis(250),
            "an unrelated 20 ms timer waited {elapsed:?} behind worker checkout"
        );
    }

    #[test]
    fn rejected_completion_preserves_the_previous_state() -> Result<(), Box<dyn std::error::Error>>
    {
        let service = Service::default().with_worker_receipts();
        let mut state = RemoteState::pristine(service.clone());
        state.view.snapshot = vec![1, 2, 3];
        state.view.metadata.names.insert("previous".into());
        let snapshot = OpaqueVmState::seal(
            VmStateKind::Snapshot,
            VmOwner::new("completed-cell"),
            lash_vm::vm_contract_versions(),
            vec![4, 5, 6],
        );
        let mismatched = CellCompletion {
            outcome: lash_vm::ExecutionOutcome::Finished(Value::Number(42.0)),
            state: EncodedPayload(rmp_serde::to_vec_named(&StateMetadata {
                definition_ids: [
                    lash_core_execution::ProcessDefinitionId::from_sha256_digest([7; 32]),
                ]
                .into(),
                ..Default::default()
            })?),
        };
        for bytes in [vec![0xc1], rmp_serde::to_vec_named(&mismatched)?] {
            assert!(matches!(
                state.install_completion(&snapshot, &EncodedPayload(bytes)),
                Err(crate::PoolError::Infrastructure(
                    InfrastructureOutcome::ProtocolViolation {
                        breach: lash_vm_protocol::ProtocolBreach::Frame { .. }
                            | lash_vm_protocol::ProtocolBreach::Sequence {
                                fault:
                                    lash_vm_protocol::SequenceFault::CompletionDefinitionsMismatch,
                            }
                    }
                ))
            ));
            assert_eq!(state.bytes(), Some([1, 2, 3].as_slice()));
            assert_eq!(state.binding_names().collect::<Vec<_>>(), ["previous"]);
        }
        assert!(service.worker_receipts().is_empty());
        Ok(())
    }
}
