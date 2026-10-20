//! Pure work a worker does for its parent: lowering and printing source.
//! None of it has parent authority, and all of it reads guest-controlled input, so it runs in the
//! worker's crash domain.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash_kernel_doc::{EffectName, Name, Site};
use lash_vm_protocol::{EncodedPayload, FrameEpoch, OwnerEpoch, VmOwner};
use serde::{Deserialize, Serialize};

use crate::{ExecutionBudget, PoolConfig, PoolError, WorkerEntry, WorkerPool};
use lash_core_store::checkpoint_graph::stage::StageFault;
use lash_vm_broker::kernel::pages::SpoolRoot;

mod source;
pub use source::SourceServiceError;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Discover free tool calls with the dialect's lexical resolver.
    ToolCalls {
        dialect: String,
        source: String,
        bindings: BTreeSet<Name>,
        functions: BTreeMap<Name, LowerFunction>,
        /// The operations the cell is offered.
        #[serde(with = "lash_kernel_dialect::operation_keys")]
        operations: lash_kernel_dialect::Operations,
        helpers: u32,
    },
    /// Lower `source`, written in `dialect`, to a kernel document against
    /// the operations the host offers and the session bindings in scope.
    Lower {
        dialect: String,
        source: String,
        /// The operations the cell is offered: each a catalog tool, a
        /// deferred grant or an unavailable note. The worker composes the
        /// cell's namespaces from them.
        #[serde(with = "lash_kernel_dialect::operation_keys")]
        operations: lash_kernel_dialect::Operations,
        bindings: BTreeSet<Name>,
        /// The functions the session holds: the document declares the
        /// ones the source names.
        functions: BTreeMap<Name, LowerFunction>,
        /// The helper release whose names the source resolves against: the
        /// newest every live node holds (FIG-5799).
        helpers: u32,
    },
    /// Lower a host-authored workflow module in the worker, in the dialect
    /// of `language`, under the shared workflow contract. Without `limits`
    /// the dialect's standard source policy applies.
    ImportWorkflow {
        language: lash_kernel_doc::SourceLanguage,
        source: String,
        /// The operations the workflow is offered.
        #[serde(with = "lash_kernel_dialect::operation_keys")]
        operations: lash_kernel_dialect::Operations,
        limits: Option<lash_kernel_dialect::workflow::WorkflowSourceLimits>,
    },
    /// Print `document` (its token body) in the selected generated format,
    /// spelling `captions` of it where the dialect can.
    Print {
        format: lash_kernel_dialect::SourceFormat,
        #[serde(with = "serde_bytes")]
        document: Vec<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        captions: Option<lash_kernel_doc::Captions>,
        options: lash_kernel_dialect::PrintOptions,
    },
    /// Read exported source back against `base` (its token body), the
    /// document it was printed from, without admission or helper resolution.
    Read {
        format: lash_kernel_dialect::SourceFormat,
        source: String,
        #[serde(with = "serde_bytes")]
        base: Vec<u8>,
        limits: lash_kernel_dialect::GeneratedSourceLimits,
    },
}

/// A saved function as lowering reads it: its code and what it requires.
/// Its frozen captures and token members stay in heap pages; lowering never
/// reads them (S8), so they do not cross.
///
/// It is stored in the session's function record, and crosses to the
/// worker that lowers a cell, as its description: one token body that
/// holds the document's own body verbatim ([`Self::description`]), so
/// neither hop encodes the document again.
#[derive(Clone, Debug)]
pub struct LowerFunction {
    pub name: Name,
    pub document: lash_kernel_doc::IdentifiedDocument,
    pub helper_owners: BTreeSet<Name>,
    pub references: BTreeSet<Name>,
    pub written: Option<lash_kernel_dialect::Written>,
    /// The tag of the function value that held it, when one did.
    pub token: Option<String>,
}

impl From<&lash_kernel_dialect::SavedFunction> for LowerFunction {
    fn from(function: &lash_kernel_dialect::SavedFunction) -> Self {
        Self {
            name: function.name.clone(),
            document: lash_kernel_doc::IdentifiedDocument::new(function.document.clone()),
            helper_owners: function.helper_owners.clone(),
            references: function.references.clone(),
            written: function.written.clone(),
            token: function.token.as_ref().map(|token| token.tag.clone()),
        }
    }
}

impl LowerFunction {
    /// The saved function lowering installs: no captures, which only a
    /// call instantiates, and no release, which only the machine pins.
    #[must_use]
    pub fn into_saved(self) -> lash_kernel_dialect::SavedFunction {
        lash_kernel_dialect::SavedFunction {
            name: self.name,
            document: self.document.into_document(),
            captures: BTreeMap::new(),
            release: lash_kernel_doc::heap::PageRef {
                id: lash_kernel_doc::heap::PageId([0; 32]),
                grant: lash_kernel_doc::heap::PageGrant::NONE,
            },
            helper_owners: self.helper_owners,
            references: self.references,
            written: self.written,
            token: self.token.map(|tag| lash_kernel_dialect::Token {
                tag,
                before: Vec::new(),
                after: Vec::new(),
            }),
        }
    }

    /// The function's description: a token sequence of its name, its
    /// document's token body (copied, not encoded), its helper owners, its
    /// references, what its source wrote and its token's tag.
    #[must_use]
    pub fn description(&self) -> Vec<u8> {
        use lash_kernel_doc::graph_tokens::{TokenCodec as _, TokenWriter};
        let mut out = TokenWriter::with_capacity(self.document.body().len() + 64);
        out.seq(6);
        self.name.encode(&mut out);
        out.raw(self.document.body());
        self.helper_owners.encode(&mut out);
        self.references.encode(&mut out);
        self.written.encode(&mut out);
        self.token.encode(&mut out);
        out.into_bytes()
    }

    /// Reads what [`Self::description`] wrote, under `limits`: the
    /// document is decoded once, and identified by its body's digest.
    ///
    /// # Errors
    ///
    /// Any token the description does not hold.
    pub fn from_description(
        description: &[u8],
        limits: lash_kernel_doc::KernelDataLimits,
    ) -> Result<Self, lash_kernel_doc::graph_tokens::TokenError> {
        use lash_kernel_doc::graph_tokens::{TokenCodec as _, TokenReader};
        let mut input = TokenReader::new(description).with_data_limits(limits);
        input.expect_struct(6)?;
        let name = Name::decode(&mut input)?;
        let document = lash_kernel_doc::IdentifiedDocument::decode(input.tree()?, limits)?;
        let function = Self {
            name,
            document,
            helper_owners: BTreeSet::decode(&mut input)?,
            references: BTreeSet::decode(&mut input)?,
            written: Option::decode(&mut input)?,
            token: Option::decode(&mut input)?,
        };
        input.finish()?;
        Ok(function)
    }
}

/// A saved function crosses the worker IPC as its description's bytes.
impl Serialize for LowerFunction {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&self.description())
    }
}

impl<'de> Deserialize<'de> for LowerFunction {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = serde_bytes::ByteBuf::deserialize(deserializer)?;
        Self::from_description(&bytes, lash_kernel_doc::KernelDataLimits::standard())
            .map_err(serde::de::Error::custom)
    }
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
    ToolCalls(BTreeSet<String>),
    Lowered {
        /// The document's token body (`K-ID-003`).
        #[serde(with = "serde_bytes")]
        document: Vec<u8>,
        /// Where each statement of the document came from in the source,
        /// checked against both by the dialect that lowered it.
        source_map: Option<lash_kernel_doc::SourceMap>,
        /// What the source wrote of each function the document makes, by
        /// the site of its closure ([`lash_kernel_dialect::Lowered::written`]).
        #[serde(with = "lash_kernel_dialect::written_table")]
        written: BTreeMap<Site, lash_kernel_dialect::Written>,
        /// The captions the source spelled, checked against the document by
        /// the dialect that lowered it: a workflow import's
        /// ([`lash_kernel_dialect::Lowered::captions`]). A cell has none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        captions: Option<lash_kernel_doc::Captions>,
        /// What the dialect's function values are, where its sessions keep
        /// the functions a cell binds for later cells
        /// ([`lash_kernel_dialect::Dialect::function_values`]).
        function_values: Option<lash_kernel_doc::FunctionRepresentation>,
        /// Those of the request's operations offered as reads the
        /// document's `main` calls or spawns, as the checked document's linked effects count them;
        /// none when they cannot be certified (`lash_kernel_check::main_effects`).
        #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
        reads: BTreeSet<EffectName>,
    },
    Printed(lash_kernel_dialect::PrintedSource),
    Read {
        /// The document's token body.
        #[serde(with = "serde_bytes")]
        document: Vec<u8>,
        /// The captions the source spelled, of that document.
        captions: lash_kernel_doc::Captions,
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
    phases: Option<crate::phases::Recorder>,
    pool: Arc<Mutex<Option<WorkerPool>>>,
    /// The parent's staging directory, opened on first use.
    spools: Arc<std::sync::OnceLock<Result<Arc<SpoolRoot>, StageFault>>>,
    pub(crate) budget: Option<ExecutionBudget>,
    /// One execution's worker, kept from lowering its document until the
    /// run starts on it: the lowering and the run share one lease, with no
    /// reset between them (FIG-5921). Only an execution's service has one.
    retained: Option<Arc<Mutex<Option<crate::Checkout>>>>,
    /// The committed pages this process read, shared by every session's
    /// reads and every activation's page service (FIG-6321).
    pages: lash_vm_broker::kernel::pages::cache::PageCache,
    /// What each session's last activation touched (FIG-6321).
    hints: lash_vm_broker::kernel::pages::cache::HintBook,
    /// The runs each execution lost on this node to a worker killed from
    /// outside it, until it answers (FIG-6806).
    lost: Arc<Mutex<BTreeMap<lash_vm_broker::ExecKey, u32>>>,
}

/// How many executions' lost runs a service keeps: one that holds as many
/// forgets the first, whose next loss counts from one again.
const LOST_EXECUTIONS: usize = 1024;

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
            phases: None,
            pool: Arc::new(Mutex::new(None)),
            spools: Arc::default(),
            budget: None,
            retained: None,
            pages: lash_vm_broker::kernel::pages::cache::PageCache::default(),
            hints: lash_vm_broker::kernel::pages::cache::HintBook::default(),
            lost: Arc::default(),
        }
    }

    /// Gives every worker the same package holdings the parent uses. Select
    /// packages before cloning or starting the service so a live pool's frame
    /// cannot diverge from its configuration.
    pub fn with_packages(mut self, packages: Arc<lash_kernel_package::HeldPackages>) -> Self {
        self.config = Arc::new(PoolConfig {
            packages: Some(packages),
            ..self.config.as_ref().clone()
        });
        self.pool = Arc::new(Mutex::new(None));
        self
    }

    #[cfg(feature = "testing")]
    pub fn with_worker_receipts(mut self) -> Self {
        self.receipts = Some(Arc::new(Mutex::new(Vec::new())));
        self
    }

    /// Observe bounded effect phases, including pre-Ready startup. Select before
    /// starting or cloning the service; overflow remains explicit in the report.
    pub fn with_effect_phases(mut self, capacity: usize) -> Self {
        self.phases = Some(crate::phases::Recorder::new(capacity));
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
        // The ready pool includes the build's immutable checkpoint catalog,
        // checked. Independent cells share these nodes instead of staging the
        // library on their first park or session capture.
        lash_vm_broker::kernel::state::release_manifests()
            .and_then(|releases| releases.check())
            .map_err(|error| {
                PoolError::breach(lash_vm_protocol::ProtocolBreach::Machine {
                    detail: lash_vm_protocol::Detail::new(error),
                })
            })?;
        let pool = WorkerPool::new_observed(self.config.as_ref().clone(), self.phases.clone())?;
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

    /// The staging directory this parent's activations open their spools
    /// under ([`PoolConfig::staging_dir`]), opened on first use with the
    /// configured per-spool byte quota.
    ///
    /// # Errors
    ///
    /// The directory's fault, which every later call answers too.
    pub fn spools(&self) -> Result<Arc<SpoolRoot>, StageFault> {
        self.spools
            .get_or_init(|| {
                #[expect(
                    clippy::disallowed_methods,
                    reason = "PoolConfig::staging_dir documents the system temporary directory as the default for a host that names none"
                )]
                let dir = self
                    .config
                    .staging_dir
                    .clone()
                    .unwrap_or_else(|| std::env::temp_dir().join("lash-staging"));
                SpoolRoot::open(&dir, self.config.staging_quota_bytes).map(Arc::new)
            })
            .clone()
    }

    /// The worker this execution kept from lowering its document, when its
    /// lease runs under these epochs.
    pub(crate) fn take_retained(
        &self,
        owner_epoch: OwnerEpoch,
        frame_epoch: FrameEpoch,
    ) -> Option<crate::Checkout> {
        let mut slot = self
            .retained
            .as_ref()?
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot
            .as_ref()
            .is_some_and(|checkout| checkout.epochs() == (owner_epoch, frame_epoch))
        {
            slot.take()
        } else {
            None
        }
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
    ToolCalls,
    Lower,
    ImportWorkflow,
    Print,
    Read,
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
        /// execution: its attempts and CPU count from zero, and its held
        /// wall time from `held`, what the execution's saved state says it
        /// already held workers for (FIG-6786).
        fn begin_execution(&self, held: std::time::Duration) -> Service;

        fn request_accounted(
            &self,
            request: Request,
        ) -> impl Future<Output = Result<Response, PoolError>> + Send;

        /// [`ServiceRuntimeOps::request_accounted`] for the lowering of the
        /// document this execution runs next: the worker that lowers it is
        /// kept for the run, which starts on the same lease with no reset
        /// between (FIG-5921). The execution gives back a worker it kept
        /// and did not start with [`ServiceRuntimeOps::release_retained`].
        fn request_for_run(
            &self,
            request: Request,
        ) -> impl Future<Output = Result<Response, PoolError>> + Send;

        /// Resets the worker this execution kept for a run that never
        /// started on it, and returns it to the pool.
        fn release_retained(&self) -> impl Future<Output = ()> + Send;

        fn pool_accounted(&self) -> impl Future<Output = Result<WorkerPool, PoolError>> + Send;

        /// Whether this execution would have a worker now, with no wait in
        /// the pool's queue: it kept the worker that lowered its document,
        /// or the pool has one idle, or room to start one (FIG-6000). A
        /// read of the moment: another execution may take the worker first.
        fn worker_ready(&self) -> bool;

        /// What each session's last activation read, warmed and sealed.
        fn page_hints(&self) -> lash_vm_broker::kernel::pages::cache::HintBook;

        /// The committed pages this process read.
        fn page_cache(&self) -> lash_vm_broker::kernel::pages::cache::PageCache;

        /// Counts a run of `exec` lost to a worker killed from outside it,
        /// and answers how many of its runs this node has lost so, this
        /// one included (FIG-6806).
        fn lost_run(&self, exec: &lash_vm_broker::ExecKey) -> u32;

        /// Forgets `exec`'s lost runs: it answered.
        fn forget_lost_runs(&self, exec: &lash_vm_broker::ExecKey);
    }

    #[doc(hidden)]
    impl ServiceRuntimeOps for Service {
        fn page_hints(&self) -> lash_vm_broker::kernel::pages::cache::HintBook {
            self.hints.clone()
        }

        fn page_cache(&self) -> lash_vm_broker::kernel::pages::cache::PageCache {
            self.pages.clone()
        }

        fn execution_budget(&self) -> Option<&ExecutionBudget> {
            self.budget.as_ref()
        }

        fn lost_run(&self, exec: &lash_vm_broker::ExecKey) -> u32 {
            let mut lost = self
                .lost
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !lost.contains_key(exec) && lost.len() >= LOST_EXECUTIONS {
                lost.pop_first();
            }
            let runs = lost.entry(exec.clone()).or_default();
            *runs = runs.saturating_add(1);
            *runs
        }

        fn forget_lost_runs(&self, exec: &lash_vm_broker::ExecKey) {
            self.lost
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(exec);
        }

        fn begin_execution(&self, held: std::time::Duration) -> Service {
            let mut service = self.clone();
            service.budget = Some(ExecutionBudget::resumed(held));
            service.retained = Some(Arc::new(Mutex::new(None)));
            service
        }

        async fn request_accounted(&self, request: Request) -> Result<Response, PoolError> {
            self.request_off_runtime(request, false).await
        }

        async fn request_for_run(&self, request: Request) -> Result<Response, PoolError> {
            self.request_off_runtime(request, true).await
        }

        async fn release_retained(&self) {
            let Some(checkout) = self.retained.as_ref().and_then(|slot| {
                slot.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
            }) else {
                return;
            };
            let _held = CallHeld::of(Some(self));
            // A reset that fails has discarded the worker; the execution's
            // answer is already decided.
            let _ = tokio::task::spawn_blocking(move || checkout.release()).await;
        }

        fn worker_ready(&self) -> bool {
            let kept = self.retained.as_ref().is_some_and(|slot| {
                slot.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_some()
            });
            if kept {
                return true;
            }
            let pool = self
                .pool
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pool.as_ref().is_none_or(|pool| {
                let stats = pool.stats();
                stats.idle > 0 || stats.workers < self.config.worker_capacity()
            })
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
        #[expect(
            clippy::disallowed_methods,
            reason = "the accounted async seam isolates blocking checkout and IPC on Tokio's blocking pool"
        )]
        async fn request_off_runtime(
            &self,
            request: Request,
            for_run: bool,
        ) -> Result<Response, PoolError> {
            let service = self.clone();
            let _held = CallHeld::of(Some(self));
            tokio::task::spawn_blocking(move || service.request_blocking(request, for_run))
                .await
                .map_err(|error| {
                    PoolError::breach(lash_vm_protocol::ProtocolBreach::Panicked {
                        detail: lash_vm_protocol::Detail::new(error),
                    })
                })?
        }

        fn request_blocking(&self, request: Request, for_run: bool) -> Result<Response, PoolError> {
            if let Request::Lower { source, .. } | Request::ToolCalls { source, .. } = &request
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
                    Request::ToolCalls { .. } => WorkerPath::ToolCalls,
                    Request::Lower { .. } => WorkerPath::Lower,
                    Request::ImportWorkflow { .. } => WorkerPath::ImportWorkflow,
                    Request::Print { .. } => WorkerPath::Print,
                    Request::Read { .. } => WorkerPath::Read,
                },
                &worker,
            )?;
            let response =
                worker.prepare(VmOwner::new("pure-worker-work"), EncodedPayload(bytes))?;
            lash_vm_protocol::FrameCodec::new(self.config.protocol.decode)
                .with_detail_limit(self.config.tuning.diagnostic_bytes)
                .check_payload(&response.0)?;
            let response = rmp_serde::from_slice(&response.0).map_err(|error| {
                PoolError::payload(lash_vm_protocol::PayloadKind::ServiceResponse, error)
            })?;
            // The worker that lowered the run's document starts the run.
            if for_run
                && matches!(response, Response::Lowered { .. })
                && let Some(slot) = &self.retained
            {
                let mut slot = slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if slot.is_none() {
                    *slot = Some(worker);
                    return Ok(response);
                }
            }
            worker.release()?;
            Ok(response)
        }
    }
}
