//! Pure compiler and guest-state operations performed by the worker.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_vm::{LashVmHostEnvironment, Record, Value};
use lash_vm_protocol::{EncodedPayload, FrameEpoch, OwnerEpoch, VmOwner};
use serde::{Deserialize, Serialize};

use crate::{ExecutionBudget, PoolConfig, PoolError, WorkerEntry, WorkerPool};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    References {
        source: String,
    },
    VerifyArtifact {
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    InspectArtifact {
        module_ref: lash_vm::ModuleRef,
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    /// Admit a workflow document: reconstruct its IR and link it against
    /// `environment`. No dialect front-end runs.
    AdmitDocument {
        graph: Box<lash_vm::WorkflowGraph>,
        environment: LashVmHostEnvironment,
    },
    ContinuationInfo {
        bytes: Vec<u8>,
    },
    /// Whether this worker's VM reads `state`: its contract versions against
    /// the VM's read ranges, then its bytes through the VM's own decoder.
    /// A holder of parked state asks before it hands the state to a run.
    CheckState {
        state: lash_vm_protocol::OpaqueVmState,
    },
    #[cfg(feature = "testing")]
    ContinuationProbe {
        bytes: Vec<u8>,
        remove_first_reference: bool,
    },
    CreateDefinition {
        source: String,
        environment: LashVmHostEnvironment,
    },
    LinkAst {
        program: lash_vm::Program,
        environment: LashVmHostEnvironment,
    },
    CompileModule {
        source: String,
        environment: LashVmHostEnvironment,
        cell: bool,
    },
    OpaqueBindings {
        snapshot: serde_bytes::ByteBuf,
        config: lash_vm::BindingSummaryConfig,
    },
    State {
        snapshot: Option<serde_bytes::ByteBuf>,
        action: StateAction,
    },
    Restore {
        header: serde_bytes::ByteBuf,
        globals: BTreeMap<String, serde_bytes::ByteBuf>,
        fleet: u32,
    },
    Capture {
        snapshot: serde_bytes::ByteBuf,
        baseline: BTreeMap<String, String>,
        fleet: u32,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StateAction {
    Inspect,
    Insert {
        name: String,
        #[serde(with = "lash_vm::effect_value")]
        value: Value,
    },
    Remove {
        names: BTreeSet<String>,
    },
    Defaults {
        #[serde(with = "lash_vm::effect_value::map")]
        values: BTreeMap<String, Value>,
        protected: BTreeSet<String>,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateMetadata {
    pub definition_ids: BTreeSet<lash_core_execution::ProcessDefinitionId>,
    #[serde(with = "lash_vm::effect_value::record")]
    pub globals: Record,
    pub names: BTreeSet<String>,
    pub expired: BTreeSet<String>,
    pub opaque: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateView {
    #[serde(with = "serde_bytes")]
    pub snapshot: Vec<u8>,
    pub metadata: StateMetadata,
}

/// A resident cell's result and metadata, emitted with its opaque snapshot.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CellCompletion {
    pub outcome: lash_vm::ExecutionOutcome,
    /// Independently bounded, as the separate state-view response was.
    pub state: EncodedPayload,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    References(BTreeSet<String>),
    ArtifactVerification(ArtifactVerification),
    Module(Box<CompiledModule>),
    Definition(CreatedDefinition),
    Artifact(crate::InspectedArtifact),
    Admitted(Box<crate::AdmittedDocument>),
    AdmissionRefused(lash_vm::WorkflowAdmissionRefusal),
    ArtifactRefused(lash_vm::ModuleArtifactRefusal),
    CompileRefused {
        error: lash_vm::ModuleCompileError,
        policy: bool,
    },
    ContinuationInfo {
        iterator_count: usize,
    },
    /// The answer to [`Request::CheckState`]: the refusal a run handed the
    /// state would meet, or `None` when the VM reads it.
    StateCheck {
        refusal: Option<lash_vm_protocol::RunRefusal>,
    },
    #[cfg(feature = "testing")]
    ContinuationProbe {
        bytes: Vec<u8>,
        closure_root: bool,
    },
    OpaqueBindings(Vec<(String, String)>),
    State(StateView),
    Captured(Capture),
    Restored {
        view: StateView,
        baseline: BTreeMap<String, String>,
    },
    Refused {
        message: String,
        policy: bool,
    },
    SnapshotRefused(lash_vm::SnapshotDecodeError),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ArtifactVerification {
    Match,
    Refused(lash_vm::ModuleArtifactRefusal),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Capture {
    pub definition_ids: BTreeSet<lash_core_execution::ProcessDefinitionId>,
    #[serde(with = "serde_bytes")]
    pub header: Vec<u8>,
    pub fragments: BTreeMap<String, lash_vm::DurableFragment>,
    pub baseline: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreatedDefinition {
    pub draft: lash_core_execution::ProcessDefinitionDraft,
    pub signature: lash_core_execution::ProcessSignature,
    pub process_name: String,
    pub module: lash_core_execution::DeclaredModuleArtifact,
}

/// A module compiled and semantically verified by a worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledModule {
    pub module_ref: lash_vm::ModuleRef,
    pub host_requirements_ref: lash_vm::HostRequirementsRef,
    pub artifact: crate::InspectedArtifact,
    pub introspection: lash_vm::ModuleIntrospection,
}
impl CompiledModule {
    /// Decode a fixture's artifact for low-level VM assertions.
    #[cfg(feature = "testing")]
    pub fn into_fixture_output(self) -> Result<lash_vm::ModuleCompileOutput, PoolError> {
        let artifact =
            lash_vm::ModuleArtifact::from_store_bytes(&self.artifact.bytes).map_err(|error| {
                PoolError::refused(lash_vm_protocol::RunRefusal::Undecodable {
                    input: lash_vm_protocol::RunInput::Artifact,
                    detail: lash_vm_protocol::Detail::new(error),
                })
            })?;
        Ok(lash_vm::ModuleCompileOutput {
            module_ref: self.module_ref,
            host_requirements_ref: self.host_requirements_ref,
            artifact,
            introspection: self.introspection,
        })
    }
}

/// What holds a simulation's virtual clock while a worker call is in
/// flight: it is called as each call begins, and what it answers is held
/// until the worker has answered. A worker runs off the host's runtime, so
/// a simulation that waits only on its runtime's tasks would move its clock
/// while the worker still computes.
#[doc(hidden)]
#[cfg(feature = "testing")]
pub type CallHold = Arc<dyn Fn() -> Box<dyn std::any::Any + Send> + Send + Sync>;

/// One worker call's hold, if its service has a [`CallHold`]: released when
/// dropped.
pub(crate) struct CallHeld {
    #[cfg(feature = "testing")]
    _held: Option<Box<dyn std::any::Any + Send>>,
}

impl CallHeld {
    /// The hold of one call through `service`.
    pub(crate) fn of(service: Option<&Service>) -> Self {
        #[cfg(not(feature = "testing"))]
        let _ = service;
        Self {
            #[cfg(feature = "testing")]
            _held: service
                .and_then(|service| service.call_hold.as_ref())
                .map(|hold| hold()),
        }
    }
}

/// One host-owned worker service. Its configuration is inspectable and has
/// no in-parent execution alternative.
#[derive(Clone)]
pub struct Service {
    #[cfg(feature = "testing")]
    receipts: Option<Arc<Mutex<Vec<WorkerReceipt>>>>,
    #[cfg(feature = "testing")]
    call_hold: Option<CallHold>,
    config: Arc<PoolConfig>,
    pool: Arc<Mutex<Option<WorkerPool>>>,
    budget: Option<ExecutionBudget>,
}
impl Service {
    /// Select the helper executable explicitly, using the RLM/process bounds.
    pub fn subprocess(executable: impl Into<std::path::PathBuf>) -> Self {
        Self::new(PoolConfig::rlm(WorkerEntry::helper(executable)))
    }

    pub fn new(config: PoolConfig) -> Self {
        Self {
            #[cfg(feature = "testing")]
            receipts: None,
            #[cfg(feature = "testing")]
            call_hold: None,
            config: Arc::new(config),
            pool: Arc::new(Mutex::new(None)),
            budget: None,
        }
    }
    #[cfg(feature = "testing")]
    pub fn with_worker_receipts(mut self) -> Self {
        self.receipts = Some(Arc::new(Mutex::new(Vec::new())));
        self
    }

    /// This service, holding `hold` for each worker call while it is in
    /// flight. Hidden from docs: simulation support, not dialect surface.
    #[doc(hidden)]
    #[cfg(feature = "testing")]
    pub fn with_call_hold(mut self, hold: CallHold) -> Self {
        self.call_hold = Some(hold);
        self
    }

    /// The worker pool this service checks workers out of, started on first
    /// use. A host calls it once at startup to prewarm the pool before any
    /// credentials load.
    pub fn pool(&self) -> Result<WorkerPool, PoolError> {
        let mut slot = self
            .pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pool) = slot.as_ref() {
            return Ok(pool.clone());
        }
        let pool = WorkerPool::new(self.config.as_ref().clone())?;
        *slot = Some(pool.clone());
        Ok(pool)
    }

    /// The workers this service checked out, when receipts are recorded.
    /// Hidden from docs: test support, not dialect surface.
    #[doc(hidden)]
    #[cfg(feature = "testing")]
    pub fn worker_receipts(&self) -> Vec<WorkerReceipt> {
        self.receipts
            .as_ref()
            .map(|receipts| {
                receipts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            })
            .unwrap_or_default()
    }
    #[cfg(feature = "testing")]
    pub(crate) fn record_worker(
        &self,
        path: WorkerPath,
        worker: &crate::Checkout,
    ) -> Result<(), PoolError> {
        if let Some(receipts) = &self.receipts {
            let pid = worker.pid().ok_or_else(PoolError::eof)?;
            let mut receipts = receipts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if receipts.len() >= 1024 {
                return Err(PoolError::breach(
                    lash_vm_protocol::SequenceFault::ReceiptProbeOverflow,
                ));
            }
            receipts.push(WorkerReceipt { path, pid });
        }
        Ok(())
    }
    pub fn config(&self) -> &PoolConfig {
        &self.config
    }
}
impl Default for Service {
    fn default() -> Self {
        // Hosts may select the shipped helper explicitly; the default is the
        // helper shipped beside the host executable. Tests name its runfile.
        #[expect(
            clippy::disallowed_methods,
            reason = "locate the packaged worker beside the host"
        )]
        let executable = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("lash-vm-worker")))
            .unwrap_or_default();
        #[cfg(any(test, feature = "testing"))]
        let executable = crate::testing::worker_executable(executable);
        Self::subprocess(executable)
    }
}

#[cfg(feature = "testing")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkerPath {
    References,
    Compile,
    CreateDefinition,
    Admit,
    Artifact,
    State,
    Cell,
    Process,
}
#[cfg(feature = "testing")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerReceipt {
    pub path: WorkerPath,
    pub pid: u32,
}

/// Runtime-only operations on a [`Service`]: the pool, requests, and the
/// per-execution budget the Lash VM runtime and the RLM protocol drive a
/// worker through. A dialect only constructs a service; these members are
/// the cross-crate runtime seam, which the `lash` facade does not re-export,
/// and the impl is hidden from docs because it is support plumbing rather
/// than dialect surface (ADR 0051).
pub mod runtime_ops {
    use std::future::Future;

    use super::*;
    use crate::WorkerPoolRuntimeOps as _;

    pub trait ServiceRuntimeOps {
        fn execution_budget(&self) -> Option<&ExecutionBudget>;

        /// This service with a fresh budget, shared by every checkout of one
        /// execution: its attempts and CPU count from zero.
        fn begin_execution(&self) -> Service;

        fn request_accounted(
            &self,
            request: Request,
        ) -> impl Future<Output = Result<Response, PoolError>> + Send;

        fn inspect_artifact(
            &self,
            store: &lash_vm::LashVmArtifacts,
            module_ref: &lash_vm::ModuleRef,
        ) -> impl Future<
            Output = Result<
                Option<crate::InspectedArtifact>,
                lash_core_execution::ArtifactStoreError,
            >,
        > + Send;

        /// `graph` admitted against `environment`, or the typed reason it
        /// is not.
        fn admit_document(
            &self,
            graph: lash_vm::WorkflowGraph,
            environment: LashVmHostEnvironment,
        ) -> impl Future<
            Output = Result<
                Result<crate::AdmittedDocument, lash_vm::WorkflowAdmissionRefusal>,
                PoolError,
            >,
        > + Send;

        fn pool_accounted(&self) -> impl Future<Output = Result<WorkerPool, PoolError>> + Send;
    }

    fn inspection_error(error: PoolError) -> lash_core_execution::ArtifactStoreError {
        match error {
            PoolError::Infrastructure(lash_vm_protocol::InfrastructureOutcome::RunRefused {
                refusal: lash_vm_protocol::RunRefusal::UnusableSchema { source },
            }) => lash_core_execution::ArtifactStoreError::UnusableSchema { source },
            PoolError::CheckoutTimedOut => {
                lash_core_execution::ArtifactStoreError::WorkerCheckoutTimedOut
            }
            error => lash_core_execution::ArtifactStoreError::Backend(error.to_string()),
        }
    }

    #[doc(hidden)]
    impl ServiceRuntimeOps for Service {
        fn execution_budget(&self) -> Option<&ExecutionBudget> {
            self.budget.as_ref()
        }

        fn begin_execution(&self) -> Service {
            let mut service = self.clone();
            service.budget = Some(ExecutionBudget::default());
            service
        }

        #[expect(
            clippy::disallowed_methods,
            reason = "the accounted async seam isolates blocking checkout and IPC on Tokio's blocking pool"
        )]
        async fn request_accounted(&self, request: Request) -> Result<Response, PoolError> {
            let service = self.clone();
            let _held = CallHeld::of(Some(self));
            tokio::task::spawn_blocking(move || service.request_blocking(request))
                .await
                .map_err(|error| {
                    PoolError::breach(lash_vm_protocol::ProtocolBreach::Panicked {
                        detail: lash_vm_protocol::Detail::new(error),
                    })
                })?
        }

        async fn inspect_artifact(
            &self,
            store: &lash_vm::LashVmArtifacts,
            module_ref: &lash_vm::ModuleRef,
        ) -> Result<Option<crate::InspectedArtifact>, lash_core_execution::ArtifactStoreError>
        {
            let Some(bytes) = store
                .store()
                .get_module_artifact(module_ref.as_str())
                .await?
            else {
                return Ok(None);
            };
            match self
                .request_accounted(Request::InspectArtifact {
                    module_ref: module_ref.clone(),
                    bytes,
                })
                .await
                .map_err(inspection_error)?
            {
                Response::Artifact(artifact) => Ok(Some(artifact)),
                Response::ArtifactRefused(refusal) => Err(refusal.into()),
                _ => Err(lash_core_execution::ArtifactStoreError::Backend(
                    "unexpected worker artifact inspection response".into(),
                )),
            }
        }

        async fn admit_document(
            &self,
            graph: lash_vm::WorkflowGraph,
            environment: LashVmHostEnvironment,
        ) -> Result<Result<crate::AdmittedDocument, lash_vm::WorkflowAdmissionRefusal>, PoolError>
        {
            match self
                .request_accounted(Request::AdmitDocument {
                    graph: Box::new(graph),
                    environment,
                })
                .await?
            {
                Response::Admitted(admitted) => Ok(Ok(*admitted)),
                Response::AdmissionRefused(refusal) => Ok(Err(refusal)),
                _ => Err(PoolError::breach(
                    lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
                )),
            }
        }

        async fn pool_accounted(&self) -> Result<WorkerPool, PoolError> {
            let service = self.clone();
            let _held = CallHeld::of(Some(self));
            tokio::task::spawn_blocking(move || service.pool())
                .await
                .map_err(|error| {
                    PoolError::breach(lash_vm_protocol::ProtocolBreach::Panicked {
                        detail: lash_vm_protocol::Detail::new(error),
                    })
                })?
        }
    }

    impl Service {
        fn request_blocking(&self, request: Request) -> Result<Response, PoolError> {
            let source = match &request {
                Request::References { source }
                | Request::CreateDefinition { source, .. }
                | Request::CompileModule { source, .. } => Some(source),
                _ => None,
            };
            if let Some(source) = source
                && source.len() as u64 > self.config.protocol.max_source_bytes
            {
                return Err(PoolError::refused(
                    lash_vm_protocol::RunRefusal::PayloadTooLarge {
                        limit: self.config.protocol.max_source_bytes,
                        size: source.len() as u64,
                    },
                ));
            }
            let bytes = rmp_serde::to_vec_named(&request).map_err(|error| {
                PoolError::payload(lash_vm_protocol::PayloadKind::ServiceRequest, error)
            })?;
            let mut worker = self.pool()?.checkout(
                bytes.len().saturating_add(4096),
                OwnerEpoch(0),
                FrameEpoch(0),
                self.budget.clone().unwrap_or_default(),
            )?;
            #[cfg(feature = "testing")]
            self.record_worker(
                match &request {
                    Request::References { .. } => WorkerPath::References,
                    Request::CompileModule { .. } | Request::LinkAst { .. } => WorkerPath::Compile,
                    Request::CreateDefinition { .. } => WorkerPath::CreateDefinition,
                    Request::AdmitDocument { .. } => WorkerPath::Admit,
                    Request::InspectArtifact { .. } | Request::VerifyArtifact { .. } => {
                        WorkerPath::Artifact
                    }
                    _ => WorkerPath::State,
                },
                &worker,
            )?;
            let response =
                worker.prepare(VmOwner::new("pure-worker-work"), EncodedPayload(bytes))?;
            lash_vm_protocol::FrameCodec::new(self.config.protocol.decode)
                .check_payload(&response.0)?;
            let response = rmp_serde::from_slice(&response.0).map_err(|error| {
                PoolError::payload(lash_vm_protocol::PayloadKind::ServiceResponse, error)
            })?;
            worker.release()?;
            Ok(response)
        }
    }
}
