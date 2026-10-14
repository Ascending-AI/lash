//! A `kernel_run`'s heap pages (FIG-6193, P1).
//!
//! A resumed run reads its parked CheckpointRoot under a lease on the
//! process's execution root, which names the page from the advance that
//! committed it; a fresh run reads only what it stages. Every page the run
//! seals stages in its activation's spool, and the step's outcome publishes
//! the root it sealed at its park: the outcome's run fact owns it from that
//! commit, and the process's root from the advance that consumes it. A
//! staging that never commits is dropped with its activation, and none of
//! its pages is ever reachable.

use std::sync::Arc;
use std::time::{Duration, Instant};

use lash_core::{ProcessId, StepPages};
use lash_core_store::checkpoint_graph::stage::StageFault;
use lash_core_store::checkpoint_graph::{CommitId, NodeId};
use lash_durable::DurableError;
use lash_durable::domain::pages::{PageStore, RootLeaseId, RootLeaseRequest, SemanticAdmission};
use lash_durable::domain::{ExecKey, RootOwner};
use lash_kernel_state::AdmittedWait;
use lash_vm_broker::kernel::pages::{
    ActivationPages, CapabilityKey, PageLimits, SharedPages, SpoolRoot,
};
use lash_vm_broker::kernel::state::{ReleaseGraph, admission_of, checkpoint_header, held_releases};
use lash_vm_broker::kernel::{KernelFailure, ParkedState, next_stage_lease};

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

impl StepActivation {
    /// The activation of `process`'s run over `step`'s store, staging in a
    /// spool of `spools`: leased on the process's root when the run resumes
    /// from `parked`, each of its waits' requests in `admissions`. Sealed
    /// pages may name the run's owner binding `binding` and the release
    /// manifests its root pins.
    ///
    /// # Errors
    ///
    /// [`RunFault`] when the process's root does not name `parked`, and the
    /// store's and the spool's failures.
    pub(crate) async fn open(
        step: &StepPages,
        process: &ProcessId,
        parked: Option<ParkedPage>,
        admissions: Vec<AdmittedWait>,
        binding: NodeId,
        releases: &ReleaseGraph,
        spools: &SpoolRoot,
        limits: PageLimits,
    ) -> Result<Self, KernelFailure> {
        let store = Arc::clone(&step.store);
        let mut references: Vec<NodeId> = releases
            .manifests
            .dialects()
            .filter_map(|dialect| releases.manifests.current(dialect))
            .map(|release| NodeId(release.0))
            .collect();
        references.push(binding);
        let (lease, resumed) = match parked {
            None => (None, None),
            Some(parked) => {
                let exec = ExecKey::Process(process.clone());
                let row = store
                    .exec_root(&exec)
                    .await
                    .map_err(KernelFailure::Lease)?
                    .ok_or_else(|| fault("the process's root does not name its parked run"))?;
                let lease = store
                    .acquire_root_lease(RootLeaseRequest {
                        owner: RootOwner::Exec(exec),
                        expected: row.root,
                        actor: step.actor.clone(),
                        epoch: step.epoch,
                        ttl_millis: i64::try_from(LEASE_TTL.as_millis()).unwrap_or(i64::MAX),
                    })
                    .await
                    .map_err(KernelFailure::Lease)?;
                let read = store.read_pages(&lease, &[parked.root]).await;
                let page = match read {
                    Ok(mut pages) if pages.len() == 1 => pages.remove(0),
                    Ok(_) => {
                        let _ = store.release_root_lease(lease.id).await;
                        return Err(fault("the process's root does not reach its parked run"));
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
                references.push(NodeId(header.release.0));
                let resumed = ParkedState {
                    exec_root: row.root,
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
            LEASE_TTL / 3,
            CapabilityKey::fresh(),
            stage.0,
            Box::new(spool),
            Instant::now() + SPAN,
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
    /// sealed at its park, pinning `release`, the release its header
    /// names: installed with the run's first outcome, required after.
    pub(crate) fn candidate(&self, root: NodeId, release: NodeId) -> RunCandidate {
        RunCandidate {
            pages: Arc::clone(&self.pages),
            store: Arc::clone(&self.store),
            root,
            release,
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

fn fault(message: &str) -> KernelFailure {
    KernelFailure::Lease(DurableError::Store(lash_durable::StoreFailure {
        kind: lash_durable::StoreFailureKind::Corrupt,
        message: RunFault(message.to_owned()).to_string(),
    }))
}

/// The CheckpointRoot a run sealed at its park, published with the step's
/// outcome from the activation's spool, with the release its header pins.
/// Dropping the last holder of the activation drops what it staged.
pub(crate) struct RunCandidate {
    pages: SharedPages,
    store: Arc<dyn PageStore>,
    root: NodeId,
    release: NodeId,
    /// Whether this is the process's first root: no earlier one pinned
    /// the release.
    first: bool,
}

fn staging(fault: StageFault) -> DurableError {
    DurableError::Store(lash_durable::StoreFailure {
        kind: lash_durable::StoreFailureKind::Unavailable,
        message: format!("the run's staging: {fault}"),
    })
}

#[async_trait::async_trait]
impl lash_core::StepPublication for RunCandidate {
    fn root(&self) -> NodeId {
        self.root
    }

    async fn publication(
        &self,
        tx: &mut lash_durable::ActorTx,
        commit: CommitId,
    ) -> Result<
        (
            Box<dyn lash_core_store::checkpoint_graph::stage::CandidateManifest>,
            SemanticAdmission,
        ),
        DurableError,
    > {
        let releases = held_releases().map_err(|failure| {
            DurableError::Store(lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Corrupt,
                message: format!("the build's releases: {failure}"),
            })
        })?;
        releases
            .pin(&*self.store, tx, self.release, self.first)
            .await
            .map_err(|unheld| {
                DurableError::Store(lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Corrupt,
                    message: unheld.to_string(),
                })
            })?;
        let manifest = self
            .pages
            .lock()
            .await
            .candidate(self.root, None, self.root, commit)
            .map_err(staging)?;
        Ok((manifest, admission_of(self.root, commit)))
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
