//! Pure compiler and guest-state operations performed by the worker.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_vm_protocol::{EncodedPayload, FrameEpoch, OwnerEpoch, VmOwner};
use lashlang::{LashlangHostEnvironment, Record, Value};
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
        module_ref: lashlang::ModuleRef,
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    TriggerCompatibility {
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
        definition: lashlang::ProcessDefinitionIdentity,
        source_type: String,
        inputs: lashlang::TriggerInputTemplate,
    },
    CompileAst {
        source: String,
        program: lashlang::Program,
        environment: LashlangHostEnvironment,
    },
    ContinuationInfo {
        bytes: Vec<u8>,
    },
    #[cfg(feature = "testing")]
    ContinuationProbe {
        bytes: Vec<u8>,
        remove_first_reference: bool,
    },
    CreateDefinition {
        source: String,
        environment: LashlangHostEnvironment,
    },
    LinkAst {
        source: String,
        program: lashlang::Program,
        environment: LashlangHostEnvironment,
    },
    CompileModule {
        source: String,
        environment: LashlangHostEnvironment,
        cell: bool,
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
        #[serde(with = "lashlang::effect_value")]
        value: Value,
    },
    Remove {
        names: BTreeSet<String>,
    },
    Defaults {
        #[serde(with = "lashlang::effect_value::map")]
        values: BTreeMap<String, Value>,
        protected: BTreeSet<String>,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateMetadata {
    pub definition_ids: BTreeSet<lash_core_execution::ProcessDefinitionId>,
    #[serde(with = "lashlang::effect_value::record")]
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
    pub outcome: lashlang::ExecutionOutcome,
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
    ArtifactRefused {
        message: String,
    },
    TriggerCompatibility(lashlang::TriggerCompatibility),
    CompileRefused {
        error: lashlang::ModuleCompileError,
        policy: bool,
    },
    ContinuationInfo {
        iterator_count: usize,
    },
    #[cfg(feature = "testing")]
    ContinuationProbe {
        bytes: Vec<u8>,
        closure_root: bool,
    },
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
    SnapshotRefused(lashlang::SnapshotDecodeError),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ArtifactVerification {
    Match,
    Undecodable { reason: String },
    IdentityMismatch { detail: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Capture {
    pub definition_ids: BTreeSet<lash_core_execution::ProcessDefinitionId>,
    #[serde(with = "serde_bytes")]
    pub header: Vec<u8>,
    pub fragments: BTreeMap<String, lashlang::DurableFragment>,
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
    pub module_ref: lashlang::ModuleRef,
    pub host_requirements_ref: lashlang::HostRequirementsRef,
    pub artifact: crate::InspectedArtifact,
    pub introspection: lashlang::ModuleIntrospection,
}
impl CompiledModule {
    /// Decode a fixture's artifact for low-level VM assertions.
    #[cfg(feature = "testing")]
    pub fn into_fixture_output(self) -> Result<lashlang::ModuleCompileOutput, PoolError> {
        let artifact = lashlang::ModuleArtifact::from_store_bytes(&self.artifact.bytes)
            .map_err(PoolError::protocol)?;
        Ok(lashlang::ModuleCompileOutput {
            module_ref: self.module_ref,
            host_requirements_ref: self.host_requirements_ref,
            artifact,
            introspection: self.introspection,
        })
    }
}

/// One host-owned worker service. Its configuration is inspectable and has
/// no in-parent execution alternative.
#[derive(Clone)]
pub struct Service {
    #[cfg(feature = "testing")]
    receipts: Option<Arc<Mutex<Vec<WorkerReceipt>>>>,
    config: Arc<PoolConfig>,
    pool: Arc<Mutex<Option<WorkerPool>>>,
    recovery: Option<Arc<dyn lash_core_execution::store::worker_recovery::WorkerRecoveryStore>>,
    budget: Option<ExecutionBudget>,
    claim: Option<Arc<lash_core_execution::store::worker_recovery::WorkerRecoveryClaim>>,
}
impl Service {
    /// Select the helper executable explicitly, using the RLM/process bounds.
    pub fn subprocess(executable: impl Into<std::path::PathBuf>) -> Self {
        let mut config = PoolConfig::standard(WorkerEntry::helper(executable));
        config.protocol.max_vm_state_bytes = 64 * 1024 * 1024;
        config.protocol.decode.max_frame_bytes = 128 * 1024 * 1024;
        config.protocol.decode.max_allocation_bytes = 256 * 1024 * 1024;
        config.max_queue_bytes = 128 * 1024 * 1024;
        Self::new(config)
    }

    pub fn new(config: PoolConfig) -> Self {
        Self {
            #[cfg(feature = "testing")]
            receipts: None,
            config: Arc::new(config),
            pool: Arc::new(Mutex::new(None)),
            recovery: None,
            budget: None,
            claim: None,
        }
    }
    #[cfg(feature = "testing")]
    pub fn with_worker_receipts(mut self) -> Self {
        self.receipts = Some(Arc::new(Mutex::new(Vec::new())));
        self
    }
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
            let pid = worker
                .pid()
                .ok_or_else(|| PoolError::protocol("worker receipt has no live child"))?;
            let mut receipts = receipts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if receipts.len() >= 1024 {
                return Err(PoolError::protocol("worker receipt probe overflow"));
            }
            receipts.push(WorkerReceipt { path, pid });
        }
        Ok(())
    }
    pub fn with_recovery_store(
        mut self,
        store: Arc<dyn lash_core_execution::store::worker_recovery::WorkerRecoveryStore>,
    ) -> Self {
        self.recovery = Some(store);
        self
    }
    pub fn recovery_store(
        &self,
    ) -> Option<&Arc<dyn lash_core_execution::store::worker_recovery::WorkerRecoveryStore>> {
        self.recovery.as_ref()
    }
    pub fn execution_budget(&self) -> Option<&ExecutionBudget> {
        self.budget.as_ref()
    }
    pub async fn begin_execution(
        &self,
        scope: &str,
    ) -> Result<crate::RecoveryExecution, PoolError> {
        self.begin_execution_from(scope, Default::default()).await
    }
    /// Reserve `scope` for an execution that continues work earlier scopes
    /// already consumed `carried` of (ADR 0123): a process body reserves a
    /// scope per segment boundary and carries the totals its boundary
    /// recorded. The reservation starts from no less than `carried`, and the
    /// row is seeded with it before any worker launches, so a redrive of
    /// this scope counts on from the carried totals too.
    pub async fn begin_execution_from(
        &self,
        scope: &str,
        carried: lash_core_execution::store::worker_recovery::WorkerRecoveryTotals,
    ) -> Result<crate::RecoveryExecution, PoolError> {
        use lash_core_execution::store::worker_recovery::{
            WorkerRecoveryError, WorkerRecoveryLimits,
        };
        let store = self
            .recovery
            .as_ref()
            .ok_or_else(|| {
                PoolError::protocol("worker execution requires its backend recovery store")
            })?
            .clone();
        let limits = WorkerRecoveryLimits {
            max_attempts: self.config.deadlines.max_attempts,
            max_cpu_nanos: self
                .config
                .deadlines
                .cumulative_cpu
                .as_nanos()
                .try_into()
                .map_err(|_| PoolError::InvalidConfiguration)?,
        };
        let mut claim = store
            .reserve(scope, limits)
            .await
            .map_err(crate::recovery::recovery_error)?;
        let seeded = crate::recovery::at_least(claim.baseline, carried);
        if seeded != claim.baseline {
            if seeded.cpu_nanos >= limits.max_cpu_nanos {
                return Err(crate::recovery::recovery_error(
                    WorkerRecoveryError::CpuExhausted,
                ));
            }
            store
                .settle(&claim, seeded)
                .await
                .map_err(crate::recovery::recovery_error)?;
            claim.baseline = seeded;
        }
        let budget = ExecutionBudget::from_recovery(claim.baseline);
        let mut service = self.clone();
        service.budget = Some(budget.clone());
        service.claim = Some(Arc::new(claim.clone()));
        Ok(crate::RecoveryExecution {
            service,
            store,
            claim,
            budget,
        })
    }
    pub async fn mark_running(&self) -> Result<(), PoolError> {
        if let (Some(store), Some(claim)) = (&self.recovery, &self.claim) {
            store
                .mark_running(claim)
                .await
                .map_err(crate::recovery::recovery_error)?;
        }
        Ok(())
    }
    pub async fn checkpoint(&self) -> Result<(), PoolError> {
        if let (Some(store), Some(claim), Some(budget)) =
            (&self.recovery, &self.claim, &self.budget)
        {
            store
                .settle(claim, budget.recovery_totals())
                .await
                .map_err(crate::recovery::recovery_error)?;
        }
        Ok(())
    }
    pub async fn request_accounted(&self, request: Request) -> Result<Response, PoolError> {
        self.mark_running().await?;
        let response = self.request(request);
        self.checkpoint().await?;
        response
    }
    pub async fn inspect_artifact(
        &self,
        store: &lashlang::LashlangArtifacts,
        module_ref: &lashlang::ModuleRef,
    ) -> Result<Option<crate::InspectedArtifact>, lash_core_execution::ArtifactStoreError> {
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
            .map_err(|error| lash_core_execution::ArtifactStoreError::Backend(error.to_string()))?
        {
            Response::Artifact(artifact) => Ok(Some(artifact)),
            Response::ArtifactRefused { message } => {
                Err(lash_core_execution::ArtifactStoreError::Decode(message))
            }
            _ => Err(lash_core_execution::ArtifactStoreError::Backend(
                "unexpected worker artifact inspection response".into(),
            )),
        }
    }
    pub fn config(&self) -> &PoolConfig {
        &self.config
    }
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
    pub fn request(&self, request: Request) -> Result<Response, PoolError> {
        let source = match &request {
            Request::References { source }
            | Request::CreateDefinition { source, .. }
            | Request::CompileModule { source, .. }
            | Request::CompileAst { source, .. }
            | Request::LinkAst { source, .. } => Some(source),
            _ => None,
        };
        if let Some(source) = source
            && source.len() as u64 > self.config.protocol.max_source_bytes
        {
            return Err(lash_vm_protocol::InfrastructureOutcome::PayloadTooLarge {
                limit: self.config.protocol.max_source_bytes,
                size: source.len() as u64,
            }
            .into());
        }
        let bytes = rmp_serde::to_vec_named(&request).map_err(PoolError::protocol)?;
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
                Request::CompileModule { .. }
                | Request::CompileAst { .. }
                | Request::LinkAst { .. } => WorkerPath::Compile,
                Request::CreateDefinition { .. } => WorkerPath::CreateDefinition,
                Request::InspectArtifact { .. }
                | Request::VerifyArtifact { .. }
                | Request::TriggerCompatibility { .. } => WorkerPath::Artifact,
                _ => WorkerPath::State,
            },
            &worker,
        )?;
        let response = worker.prepare(VmOwner::new("pure-worker-work"), EncodedPayload(bytes))?;
        lash_vm_protocol::FrameCodec::new(self.config.protocol.decode)
            .check_payload(&response.0)?;
        let response = rmp_serde::from_slice(&response.0).map_err(PoolError::protocol)?;
        worker.release()?;
        Ok(response)
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
        let service = Self::subprocess(executable);
        #[cfg(any(test, feature = "testing"))]
        let service =
            service.with_recovery_store(Arc::new(crate::recovery::RecoveryDouble::default()));
        service
    }
}

#[cfg(feature = "testing")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkerPath {
    References,
    Compile,
    CreateDefinition,
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
