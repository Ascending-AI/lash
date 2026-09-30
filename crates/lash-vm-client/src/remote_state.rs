//! Parent-held bytes and read-only guest projections. All state operations
//! which open a snapshot run in a worker through the service.
use lash_core_execution::FleetFormat;
use lashlang::{Record, Value};
use std::collections::BTreeSet;

use crate::service::{Capture, Request, Response, Service, StateAction, StateView};

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
                definition_ids: BTreeSet::new(),
                snapshot: Vec::new(),
                globals: Record::new(),
                names: BTreeSet::new(),
                expired: BTreeSet::new(),
                opaque: Vec::new(),
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
        &self.view.globals
    }
    pub fn binding_names(&self) -> impl Iterator<Item = &str> {
        self.view.names.iter().map(String::as_str)
    }
    pub fn expired_functions(&self) -> &BTreeSet<String> {
        &self.view.expired
    }
    pub fn opaque_bindings(&self) -> Vec<(String, String)> {
        self.view.opaque.clone()
    }
    pub fn referenced_definition_ids(&self) -> BTreeSet<lash_core_execution::ProcessDefinitionId> {
        self.view.definition_ids.clone()
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
        if !self.view.names.contains(name) {
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
