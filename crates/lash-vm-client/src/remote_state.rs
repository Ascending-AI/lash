//! Parent-held bytes and read-only guest projections. All state operations
//! which open a snapshot run in a worker through the service.
use lash_core_execution::FleetFormat;
use lashlang::{Record, Value};
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
    pub fn referenced_definition_ids(&self) -> BTreeSet<lash_core_execution::ProcessDefinitionId> {
        self.view.metadata.definition_ids.clone()
    }
    /// Adopt a broker-checked completion without reopening its VM snapshot.
    pub fn install_completion(
        &mut self,
        snapshot: &lash_vm_protocol::OpaqueVmState,
        value: &lash_vm_protocol::EncodedPayload,
    ) -> Result<lashlang::ExecutionOutcome, crate::PoolError> {
        let codec = lash_vm_protocol::FrameCodec::new(
            self.service.config().entry.build.clone(),
            self.service.config().protocol.decode,
        );
        codec.check_payload(&value.0)?;
        let completion: CellCompletion =
            rmp_serde::from_slice(&value.0).map_err(crate::PoolError::protocol)?;
        codec.check_payload(&completion.state.0)?;
        let metadata: StateMetadata =
            rmp_serde::from_slice(&completion.state.0).map_err(crate::PoolError::protocol)?;
        if !metadata
            .definition_ids
            .iter()
            .eq(snapshot.definition_ids().iter())
        {
            return Err(crate::PoolError::protocol(
                "completion metadata names different definitions than its snapshot",
            ));
        }
        self.view = StateView {
            snapshot: snapshot.bytes().to_vec(),
            metadata,
        };
        Ok(completion.outcome)
    }
    pub fn install_bytes(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        match self
            .service
            .request(Request::State {
                snapshot: Some(bytes.into()),
                action: StateAction::Inspect,
            })
            .map_err(|e| e.to_string())?
        {
            Response::State(view) => {
                self.view = view;
                Ok(())
            }
            other => Err(format!("worker returned {other:?}")),
        }
    }
    pub fn insert_global(&mut self, name: impl Into<String>, value: Value) -> Result<(), String> {
        self.mutate(StateAction::Insert {
            name: name.into(),
            value,
        })
    }
    pub fn remove_global(&mut self, name: &str) -> Result<bool, String> {
        if !self.view.metadata.names.contains(name) {
            return Ok(false);
        }
        self.mutate(StateAction::Remove {
            names: BTreeSet::from([name.to_string()]),
        })?;
        Ok(true)
    }
    pub fn remove_names(&mut self, names: BTreeSet<String>) -> Result<(), String> {
        self.mutate(StateAction::Remove { names })
    }
    pub fn defaults(
        &mut self,
        values: std::collections::BTreeMap<String, Value>,
        protected: BTreeSet<String>,
    ) -> Result<(), String> {
        self.mutate(StateAction::Defaults { values, protected })
    }
    fn mutate(&mut self, action: StateAction) -> Result<(), String> {
        match self
            .service
            .request(Request::State {
                snapshot: self.bytes().map(serde_bytes::ByteBuf::from),
                action,
            })
            .map_err(|e| e.to_string())?
        {
            Response::State(view) => {
                self.view = view;
                Ok(())
            }
            other => Err(format!("worker returned {other:?}")),
        }
    }
    pub fn capture(
        &self,
        baseline: &std::collections::BTreeMap<String, String>,
        fleet: FleetFormat,
    ) -> Result<Capture, String> {
        let snapshot = match self.bytes() {
            Some(bytes) => bytes.to_vec(),
            None => match self
                .service
                .request(Request::State {
                    snapshot: None,
                    action: StateAction::Inspect,
                })
                .map_err(|e| e.to_string())?
            {
                Response::State(view) => view.snapshot,
                other => return Err(format!("worker returned {other:?}")),
            },
        };
        match self
            .service
            .request(Request::Capture {
                snapshot: snapshot.into(),
                baseline: baseline.clone(),
                fleet: fleet.version(),
            })
            .map_err(|e| e.to_string())?
        {
            Response::Captured(parts) => Ok(parts),
            other => Err(format!("worker returned {other:?}")),
        }
    }
    pub fn restore(
        &mut self,
        header: Vec<u8>,
        globals: std::collections::BTreeMap<String, Vec<u8>>,
        fleet: FleetFormat,
    ) -> Result<std::collections::BTreeMap<String, String>, RemoteRestoreError> {
        match self
            .service
            .request(Request::Restore {
                header: header.into(),
                globals: globals.into_iter().map(|(k, v)| (k, v.into())).collect(),
                fleet: fleet.version(),
            })
            .map_err(RemoteRestoreError::Worker)?
        {
            Response::Restored { view, baseline } => {
                self.view = view;
                Ok(baseline)
            }
            Response::SnapshotRefused(error) => Err(RemoteRestoreError::Snapshot(error)),
            other => Err(RemoteRestoreError::Worker(crate::PoolError::protocol(
                format!("worker returned {other:?}"),
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
    Snapshot(lashlang::SnapshotDecodeError),
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
            lashlang::vm_contract_versions(),
            lashlang::LASHLANG_SNAPSHOT_VERSION,
            vec![4, 5, 6],
        );
        let mismatched = CellCompletion {
            outcome: lashlang::ExecutionOutcome::Finished(Value::Number(42.0)),
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
                    InfrastructureOutcome::ProtocolViolation { .. }
                ))
            ));
            assert_eq!(state.bytes(), Some([1, 2, 3].as_slice()));
            assert_eq!(state.binding_names().collect::<Vec<_>>(), ["previous"]);
        }
        assert!(service.worker_receipts().is_empty());
        Ok(())
    }
}
