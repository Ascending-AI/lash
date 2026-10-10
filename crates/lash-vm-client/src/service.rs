//! Pure work a worker does for its parent: lowering and printing source.
//! None of it has parent authority, and all of it reads guest-controlled input, so it runs in the
//! worker's crash domain.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_kernel_doc::{EffectName, Name, Signature};
use lash_vm_protocol::{EncodedPayload, FrameEpoch, OwnerEpoch, VmOwner};
use serde::{Deserialize, Serialize};

use crate::{ExecutionBudget, PoolConfig, PoolError, WorkerEntry, WorkerPool};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Lower `source`, written in `dialect`, to a kernel document against
    /// the effects the host supplies and the session bindings in scope.
    Lower {
        dialect: String,
        source: String,
        effects: BTreeMap<EffectName, Signature>,
        /// Namespace roots from the catalog, before flattening tool names.
        tool_roots: BTreeSet<Name>,
        /// The turn controls each control-declaring effect declares.
        controls: BTreeMap<EffectName, BTreeSet<lash_kernel_dialect::EffectControl>>,
        bindings: BTreeSet<Name>,
        /// The functions the session holds: the document declares the
        /// ones the source names.
        functions: BTreeMap<Name, lash_kernel_dialect::SavedFunction>,
        /// The helper release whose names the source resolves against: the
        /// newest every live node holds (FIG-5799).
        helpers: u32,
    },
    /// Print `document` (its JSON encoding) as source in `dialect`.
    Print {
        dialect: String,
        #[serde(with = "serde_bytes")]
        document: Vec<u8>,
    },
}

/// A front end's or printer's typed refusal of a source or a document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialectRefusal {
    pub code: String,
    pub message: String,
    /// The byte span in the source, when the refusal has one.
    pub span: Option<(usize, usize)>,
    /// Whether the dialect chose not to support the construct, as opposed
    /// to the source being malformed.
    pub unsupported: bool,
    pub repairs: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Lowered {
        /// The document's JSON encoding.
        #[serde(with = "serde_bytes")]
        document: Vec<u8>,
        /// Its annotations' JSON encoding.
        #[serde(with = "serde_bytes")]
        annotations: Vec<u8>,
        /// What the dialect's function values are, where its sessions keep
        /// the functions a cell binds for later cells
        /// ([`lash_kernel_dialect::Package::function_values`]).
        function_values: Option<lash_kernel_dialect::FunctionValues>,
    },
    Printed {
        source: String,
    },
    DialectRefused(DialectRefusal),
    /// The worker has no such dialect installed.
    UnknownDialect {
        dialect: String,
    },
    /// The worker holds no such helper release.
    HelperReleaseNotHeld {
        release: u32,
    },
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
    pub(crate) budget: Option<ExecutionBudget>,
}
impl Service {
    /// Select the helper executable explicitly, using the code mode/process bounds.
    pub fn subprocess(executable: impl Into<std::path::PathBuf>) -> Self {
        Self::new(PoolConfig::codemode(WorkerEntry::helper(executable)))
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
    Lower,
    Print,
    /// A run of a document's `main`.
    Cell,
    /// A run of one of a document's entries.
    Process,
}
#[cfg(feature = "testing")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerReceipt {
    pub path: WorkerPath,
    pub pid: u32,
}

/// Runtime-only operations on a [`Service`]: the pool, requests, and the
/// per-execution budget the runtime drives a worker through. A host only
/// constructs a service; these members are the cross-crate runtime seam,
/// which the `lash` facade does not re-export, and the impl is hidden from
/// docs because it is support plumbing rather than host surface (ADR 0051).
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

        fn pool_accounted(&self) -> impl Future<Output = Result<WorkerPool, PoolError>> + Send;
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
            if let Request::Lower { source, .. } = &request
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
                    Request::Lower { .. } => WorkerPath::Lower,
                    Request::Print { .. } => WorkerPath::Print,
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
