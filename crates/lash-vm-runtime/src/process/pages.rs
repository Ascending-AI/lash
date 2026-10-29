//! A `kernel_run`'s heap pages (FIG-6193, P1).
//!
//! A resumed run reads its parked CheckpointRoot under a lease on the
//! process's execution root, which names the page from the advance that
//! committed it; a fresh run reads only what it stages. Every page the run
//! seals stages in its activation's spool, and the step's outcome publishes
//! the root it sealed at its park: the outcome's run fact owns it from that
//! commit, and the process's root from the advance that consumes it. A
//! staging that never commits is dropped with its activation, and none of
//! its pages is ever reachable. A replay (FIG-6370) reads the page a run
//! parked with under a reader's lease on the record of the run fact that
//! holds it, and publishes nothing.

use std::sync::Arc;
use std::time::Duration;

use lash_core_execution::runtime::Clock;
use lash_core_store::checkpoint_graph::stage::StageFault;
use lash_core_store::checkpoint_graph::{CommitId, EncodedNode, NodeId};
use lash_durable::DurableError;
use lash_durable::domain::pages::{LeaseFence, PageStore, RootLeaseId, RootLeaseRequest};
use lash_durable::domain::{ExecKey, RootOwner};
use lash_execution_contracts::process::{StepManifest, StepPages};
use lash_kernel_state::AdmittedWait;
use lash_vm_broker::kernel::closure::Closure;
use lash_vm_broker::kernel::pages::{
    ActivationPages, CapabilityKey, PageLimits, SharedPages, SpoolRoot,
};
use lash_vm_broker::kernel::state::checkpoint_header;
use lash_vm_broker::kernel::{KernelFailure, ParkedState, next_stage_lease};
use lash_vocabulary::identity::ProcessId;

use super::run::RunFault;
use super::state::ParkedPage;

/// How long the lease a run reads under lives between renewals.
const LEASE_TTL: Duration = Duration::from_secs(60);
/// How long all of one run's page exchanges may take.
const SPAN: Duration = Duration::from_secs(3600);

/// One `kernel_run`'s page service, the parked state it resumes and the
/// lease it read that state under.
pub(crate) struct StepActivation {
    pub(crate) pages: SharedPages,
    pub(crate) parked: Option<ParkedState>,
    store: Arc<dyn PageStore>,
    lease: Option<RootLeaseId>,
}

/// The record a run's parked page is read under, and the lease that pins it.
pub(crate) enum ParkedUnder<'a> {
    /// A live run: the process's execution root, which names the page from
    /// the advance that committed it, leased under the step's fence.
    Process(&'a StepPages),
    /// A replay: the record of the run fact `owner` that holds the page,
    /// read under a reader's lease of `ttl`, refused once the record is
    /// gone.
    Record {
        owner: RootOwner,
        record: NodeId,
        ttl: Duration,
    },
}

impl StepActivation {
    /// The activation of `process`'s run over `store`, staging in a spool
    /// of `spools`: resumed from `parked`, read under what it names, each
    /// of its waits' requests in `admissions`. Sealed pages may name the
    /// run's owner binding `binding` and `closure`, the execution closure
    /// its roots pin.
    ///
    /// # Errors
    ///
    /// [`RunFault`] when the process's root does not name `parked`, a
    /// refused lease, and the store's and the spool's failures.
    #[expect(
        clippy::too_many_arguments,
        reason = "one activation, in the parts its step names"
    )]
    pub(crate) async fn open(
        store: Arc<dyn PageStore>,
        process: &ProcessId,
        parked: Option<(ParkedPage, ParkedUnder<'_>)>,
        admissions: Vec<AdmittedWait>,
        binding: NodeId,
        closure: &Closure,
        spools: &SpoolRoot,
        clock: Arc<dyn Clock>,
        limits: PageLimits,
    ) -> Result<Self, KernelFailure> {
        let references = [closure.reference(), binding];
        let mut ttl = LEASE_TTL;
        let (lease, resumed) = match parked {
            None => (None, None),
            Some((parked, under)) => {
                let request = match under {
                    ParkedUnder::Process(step) => {
                        let exec = ExecKey::Process(process.clone());
                        let row = store
                            .exec_root(&exec)
                            .await
                            .map_err(KernelFailure::Lease)?
                            .ok_or_else(|| {
                                fault("the process's root does not name its parked run")
                            })?;
                        RootLeaseRequest {
                            owner: RootOwner::Exec(exec),
                            expected: row.root,
                            fence: LeaseFence::Actor {
                                actor: step.actor.clone(),
                                epoch: step.epoch,
                            },
                            ttl_millis: millis(ttl),
                        }
                    }
                    ParkedUnder::Record {
                        owner,
                        record,
                        ttl: reader,
                    } => {
                        ttl = reader;
                        RootLeaseRequest {
                            owner,
                            expected: record,
                            fence: LeaseFence::Reader,
                            ttl_millis: millis(ttl),
                        }
                    }
                };
                let lease = lash_durable::fenced_read::acquire_lease(store.as_ref(), request)
                    .await
                    .map_err(KernelFailure::Lease)?;
                let read = store.read_pages(&lease, &[parked.root]).await;
                let page = match read {
                    Ok(mut pages) if pages.len() == 1 => pages.remove(0),
                    Ok(_) => {
                        let _ = store.release_root_lease(lease.id).await;
                        return Err(fault("the leased record does not reach its parked run"));
                    }
                    Err(error) => {
                        let _ = store.release_root_lease(lease.id).await;
                        return Err(KernelFailure::Lease(error));
                    }
                };
                let header = match checkpoint_header(parked.root, &page.bytes) {
                    Ok(header) => header,
                    Err(failure) => {
                        let _ = store.release_root_lease(lease.id).await;
                        return Err(KernelFailure::State(failure));
                    }
                };
                let resumed = ParkedState {
                    exec_root: lease.root,
                    root: parked.root,
                    header,
                    admissions,
                };
                (Some(lease), Some(resumed))
            }
        };
        let owned = lease.as_ref().map(|lease| lease.id);
        let stage = next_stage_lease();
        let spool = spools.spool(stage)?;
        let pages = ActivationPages::new(
            Arc::clone(&store),
            lease,
            ttl / 3,
            CapabilityKey::fresh(),
            stage.0,
            Box::new(spool),
            clock,
            SPAN,
            limits,
        )
        .with_references(references);
        Ok(Self {
            pages: Arc::new(tokio::sync::Mutex::new(pages)),
            parked: resumed,
            store,
            lease: owned,
        })
    }

    /// The candidate that publishes `root`, the CheckpointRoot the run
    /// sealed at its park, pinning `closure`, the execution closure its
    /// header names: staged with the run's first outcome, required after.
    pub(crate) fn candidate(&self, root: NodeId, closure: Closure) -> RunCandidate {
        RunCandidate {
            pages: Arc::clone(&self.pages),
            root,
            closure,
            first: self.parked.is_none(),
        }
    }

    /// Ends the run's reads: the lease it took goes, and its expiry would
    /// end it as well. What the run staged stays with its candidate.
    pub(crate) async fn end_reads(&mut self) {
        if let Some(lease) = self.lease.take() {
            let _ = self.store.release_root_lease(lease).await;
        }
    }
}

fn millis(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX)
}

fn fault(message: &str) -> KernelFailure {
    KernelFailure::Lease(DurableError::Store(lash_durable::StoreFailure {
        code: None,
        kind: lash_durable::StoreFailureKind::Corrupt,
        message: RunFault(message.to_owned()).to_string(),
    }))
}

/// The CheckpointRoot a run sealed at its park, published with the step's
/// outcome from the activation's spool, with the closure its header pins.
/// Dropping the last holder of the activation drops what it staged.
pub(crate) struct RunCandidate {
    pages: SharedPages,
    root: NodeId,
    closure: Closure,
    /// Whether this is the process's first root: no earlier one staged
    /// the closure.
    first: bool,
}

fn staging(fault: StageFault) -> DurableError {
    DurableError::Store(lash_durable::StoreFailure {
        code: None,
        kind: lash_durable::StoreFailureKind::Unavailable,
        message: format!("the run's staging: {fault}"),
    })
}

#[async_trait::async_trait]
impl lash_execution_contracts::process::StepPublication for RunCandidate {
    fn root(&self) -> NodeId {
        self.root
    }

    async fn publication(
        &self,
        tx: &mut lash_durable::ActorTx,
        commit: CommitId,
    ) -> Result<StepManifest, DurableError> {
        self.closure.stage(tx, self.first);
        let mut pages = self.pages.lock().await;
        let root = EncodedNode {
            id: self.root,
            bytes: pages.spool().read(&self.root).map_err(staging)?,
        };
        let manifest = pages
            .candidate(self.root, None, self.root, commit)
            .map_err(staging)?;
        Ok(StepManifest { manifest, root })
    }

    async fn settled(&self, commit: CommitId) {
        if let Err(fault) = self.pages.lock().await.settle(commit) {
            tracing::debug!(%fault, "a published run's staging did not settle");
        }
    }

    async fn withdrawn(&self, commit: CommitId) {
        if let Err(fault) = self.pages.lock().await.spool().withdraw(commit) {
            tracing::debug!(%fault, "a refused run's candidate did not withdraw");
        }
    }

    /// The staging goes with the activation's last holder.
    async fn abandoned(&self) {}
}

/// Check the immutable root before constructing any execution registry. The
/// reader lease owns the blocking traversal and releases on cancellation or
/// refusal; no carry pages or guest instructions can precede this proof.
#[doc(hidden)]
pub async fn check_parked_helpers(
    store: Arc<dyn PageStore>,
    request: RootLeaseRequest,
    parked: NodeId,
    binding: NodeId,
    content: &lash_kernel_package::LinkedContent,
    helpers: Arc<lash_kernel_doc::BuildHelpers>,
) -> Result<lash_kernel_doc::Selection, KernelFailure> {
    use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
    use lash_kernel_state::paged::{Checkpoint, HelperCompatibilityError, PageSource as _};
    let source = super::page_carry::leased_pages(store, request)
        .await
        .map_err(|error| match error {
            lash_kernel_migrate::CarryFault::Heap(fault) => {
                KernelFailure::Open(lash_kernel_vm::OpenError::Heap(fault))
            }
            other => fault(&other.to_string()),
        })?;
    let (checkpoint, source) = tokio::task::spawn_blocking(move || {
        let mut source = source;
        let answer = source
            .read(&[PageRef {
                id: PageId(parked.0),
                grant: PageGrant::NONE,
            }])?
            .into_iter()
            .next()
            .ok_or_else(|| HeapFault::Malformed("root read returned no page".into()))??;
        let checkpoint = Checkpoint::of_answer(&answer)?;
        if checkpoint.header().owner != PageId(binding.0) {
            return Err(HelperCompatibilityError::Heap(HeapFault::Malformed(
                "checkpoint belongs to another execution owner".into(),
            )));
        }
        Ok((checkpoint, source))
    })
    .await
    .map_err(|error| fault(&error.to_string()))?
    .map_err(helper_refusal)?;
    if checkpoint.header().helper_library == helpers.identity() {
        return lash_kernel_state::paged::check_helper_compatibility(
            checkpoint.header().helper_library,
            || unreachable!("equal image never walks"),
            &helpers,
        )
        .map(|()| lash_kernel_doc::Selection::Fresh)
        .map_err(helper_refusal);
    }
    let content = content.clone();
    tokio::task::spawn_blocking(move || {
        let (documents, imports) = helper_code(&content)?;
        let (_, selection) = checkpoint.check_helper_compatibility(
            Box::new(source),
            &documents,
            &imports,
            &helpers,
        )?;
        Ok(selection)
    })
    .await
    .map_err(|error| fault(&error.to_string()))?
    .map_err(helper_refusal)
}

/// Project validated operation contexts intact. A code Import's exact Link
/// and export determines its function; operation contracts remain untouched.
type HelperCode = (
    std::collections::BTreeMap<lash_kernel_value::LinkId, lash_kernel_doc::Document>,
    std::collections::BTreeMap<
        (lash_kernel_value::LinkId, lash_kernel_value::ImportSlot),
        lash_kernel_value::CallableId,
    >,
);

pub(super) fn helper_code(
    content: &lash_kernel_package::LinkedContent,
) -> Result<HelperCode, lash_kernel_state::paged::HelperCompatibilityError> {
    use lash_kernel_doc::heap::HeapFault;
    use lash_kernel_value::package::Fill;
    let mut documents = std::collections::BTreeMap::new();
    let mut imports = std::collections::BTreeMap::new();
    for (link, instance) in &content.links {
        let document = content
            .documents
            .get(&instance.document)
            .ok_or_else(|| HeapFault::Malformed("helper code Link document is absent".into()))?;
        documents.insert(*link, document.as_ref().clone());
        for (slot, fill) in &instance.fills {
            if let Fill::Export {
                link: target,
                export,
            } = fill
                && let lash_kernel_doc::Export::Function(function) = content
                    .export(*target, export)
                    .map_err(|error| HeapFault::Malformed(error.to_string()))?
            {
                imports.insert(
                    (*link, *slot),
                    lash_kernel_value::CallableId {
                        link: *target,
                        function: function.function,
                    },
                );
            }
        }
    }
    Ok((documents, imports))
}

pub(super) fn helper_refusal(
    error: lash_kernel_state::paged::HelperCompatibilityError,
) -> KernelFailure {
    use lash_kernel_state::paged::HelperCompatibilityError;
    match error {
        HelperCompatibilityError::Heap(fault) => {
            KernelFailure::Open(lash_kernel_vm::OpenError::Heap(fault))
        }
        HelperCompatibilityError::HelperUnavailable { reason, function } => KernelFailure::Worker {
            outcome: lash_protocol_types::engine_fault::RunRefusal::Restore {
                refusal:
                    lash_protocol_types::engine_fault::KernelRestoreRefusal::HelperUnavailable {
                        reason,
                        function,
                    },
            }
            .into(),
        },
    }
}
