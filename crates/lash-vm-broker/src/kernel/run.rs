//! One kernel run, driven to its end or to a park that outlives this
//! activation.
//!
//! The machine parks; the broker commits. Each time the machine parks with
//! new requests, the machine seals its changed pages and a checkpoint root
//! into the activation's spool, and the broker publishes that candidate
//! with the admission of every request ([`DurableSnapshotStore::commit_park`]).
//! It then delivers every committed outcome the ledger stands on, in
//! whatever order they are read, and runs the machine again. A park that
//! requests nothing saves nothing: a save is owed only before effects are
//! admitted.
//!
//! A run that has a checkpoint resumes from it: the machine opens the
//! committed root under a lease on it, faulting in only the pages it
//! touches, with each wait's request from its admission; the first delivery answers,
//! from their records, every effect whose outcome committed and the saved
//! state had not consumed. Whatever ran after that save runs again; no
//! committed effect does.
//!
//! The broker does not know where the machine lives. It drives a
//! [`DrivenMachine`] that a [`Machines`] starts or resumes: lash's is a
//! machine in a resettable worker process (kernel spec §9 rule 5).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lash_core_store::checkpoint_graph::stage::{StageFault, StageLeaseId, StageSpool};
use lash_core_store::checkpoint_graph::{GraphStaging, NodeId};
use lash_durable::domain::graph::RootOwner;
use lash_durable::domain::pages::{RootLease, RootLeaseRequest};
use lash_durable::{DurableError, DurableInstant};
use lash_kernel_doc::{DocumentId, EncodeError, ErrorDatum};
use lash_kernel_vm::{
    Bound, BoundExceeded, Bounds, DeliverError, EffectRequest, End, MachineError, OpenError,
    Outcome, Request, RunError, StartError, WaitId,
};
use lash_vm_protocol::{CheckpointRefusal, InfrastructureOutcome};
use tokio_util::sync::CancellationToken;

use super::graph::{FrozenRequest, Projections};
use super::ledger::{AdmittedEffect, EffectLedger};
use super::pages::{ActivationPages, CapabilityKey, PageLimits, SharedPages, SpoolRoot};
use super::state::{GraphStateFailure, ReleaseGraph, checkpoint_header};
use super::store::{
    AdmitAs, AdmittedCall, Checkpoint, EffectAdmission, FirstPark, HostParts, ParkSave,
    ParkedState, SealedPark,
};
use crate::effects::{MemberDraft, ParentFault};
use crate::identity::CodeCallIdentities;
use crate::members::Driven;
use crate::snapshot::{DurableSnapshotStore, QuietPointRefusal};
use lash_core_execution::runtime::actor::round::SettledOutput;

/// The parent's half of a kernel run: how each effect is admitted, how a
/// committed outcome answers it, and the host's own state each save
/// publishes. Its projections read back the tool requests it froze.
#[async_trait::async_trait]
pub trait KernelEffects: Projections + Send + Sync {
    /// The execution `request` is admitted as, under `call`, the identity
    /// the parent derived for it: its tool, frozen request, policy, limit
    /// and completion wait. `Err` refuses the effect: the `perform` raises
    /// that error, which the guest may catch, and nothing is dispatched.
    /// `outstanding` counts the tasks that would make the run's end an
    /// error at this park ([`Park::outstanding`](lash_kernel_vm::Park)):
    /// `live` also counts passed list-join members that have not ended
    /// ([`Park::live`](lash_kernel_vm::Park)). A turn-ending effect must
    /// have neither outstanding nor live sibling tasks.
    async fn admit(
        &self,
        request: &EffectRequest,
        call: lash_sansio::ToolCallId,
        outstanding: u64,
        live: u64,
    ) -> Result<Result<MemberDraft, ErrorDatum>, ParentFault>;

    /// The outcome the `perform` admitted as `effect` is answered with for
    /// `output`, its execution's final record; `None` while it is not
    /// final. The default reads a completed record's payload as the result.
    /// It is called on every read of a settled wait, so it must be a
    /// function of its arguments alone: a resume reads the same records
    /// again.
    fn outcome(&self, effect: &AdmittedEffect, output: &SettledOutput) -> Option<Outcome> {
        let _ = effect;
        super::store::outcome_of(output)
    }

    /// A committed host outcome ends this run before guest code resumes.
    /// The host retains its typed terminal in its state.
    fn stop_requested(&self) -> bool {
        false
    }

    /// The run resumed from its latest park: each execution its ledger
    /// names, awaited or released, with the request its admission froze.
    ///
    /// # Errors
    ///
    /// [`ParentFault`] for an execution this host cannot run.
    fn resumed(&self, executions: &[(AdmittedEffect, FrozenRequest)]) -> Result<(), ParentFault> {
        let _ = executions;
        Ok(())
    }

    /// Stages the host's own state for a save, which admitted `admitted`:
    /// its HostRoot and CellEnvelope, each when it keeps one. Nodes an
    /// earlier root installed and this one reuses go in `reads`.
    ///
    /// # Errors
    ///
    /// [`ParentFault`] when the host's state does not stage.
    fn stage_host(
        &self,
        staging: &mut GraphStaging,
        reads: &mut BTreeSet<NodeId>,
        admitted: &[AdmittedCall],
    ) -> Result<HostParts, ParentFault> {
        let _ = (staging, reads, admitted);
        Ok(HostParts::default())
    }

    /// The save that staged the host's state committed.
    fn host_committed(&self) {}

    /// Whether a fresh run performs some effect as a read (ADR 0144): it
    /// then saves its start before any statement runs, so no read is made
    /// before the run's providers are owed.
    fn plans_reads(&self) -> bool {
        false
    }

    /// The model response that asked for this run, while its turn holds it
    /// uncommitted for the run's first save (FIG-6000): that save commits it
    /// in the same transaction. The response commits on its own once the
    /// run's first stretch outlasts one slice.
    fn held_response(&self) -> Option<&lash_core_execution::HeldResponse> {
        None
    }
}

/// A machine's park as the broker reads it: [`lash_kernel_vm::Park`]
/// less its task table, which stays in the machine's heap.
#[derive(Clone, Debug, PartialEq)]
pub struct MachinePark {
    pub requests: Vec<Request>,
    pub withdrawn: Vec<WaitId>,
    /// How many tasks would make the run's end an error at this park
    /// ([`Park::outstanding`](lash_kernel_vm::Park)).
    pub outstanding: u64,
    /// How many tasks other than `main` have not ended
    /// ([`Park::live`](lash_kernel_vm::Park)).
    pub live: u64,
}

/// Where one `run` of a driven machine stopped.
#[derive(Clone, Debug, PartialEq)]
pub enum MachineStep {
    Parked(MachinePark),
    Slice,
    Ended(End),
}

/// One run's machine, wherever it executes. Between calls the run is at a
/// safe point. Its heap reads and seals pages through the activation's
/// page service ([`SharedPages`]), which the broker publishes from.
#[async_trait::async_trait]
pub trait DrivenMachine: Send {
    /// Runs ready tasks until none is ready, the run ends or `slice` charge
    /// units are spent. With `cancel`, the run observes its cancel at its
    /// next safe point and ends [`End::Cancelled`].
    async fn run(&mut self, slice: u64, cancel: bool) -> Result<MachineStep, KernelFailure>;

    /// Hands the machine one committed outcome: the machine takes it, or
    /// drops it when its wait was withdrawn, at the latest before the next
    /// `run` runs anything.
    async fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<(), KernelFailure>;

    /// Seals the run as it stands at a park, or at its start before its
    /// first slice: its changed pages and a CheckpointRoot page, staged in
    /// the activation's spool. Returns the root page; the broker reads and
    /// checks its header before it publishes anything.
    async fn seal(&mut self) -> Result<NodeId, KernelFailure>;

    /// Gives the machine up at a clean stop: the run ended, or it stays
    /// parked beyond this activation. A machine that is only dropped was
    /// abandoned mid-run.
    async fn release(self) -> Result<(), KernelFailure>;
}

/// Where a run's machine comes from.
#[async_trait::async_trait]
pub trait Machines: Send + Sync {
    type Machine: DrivenMachine;

    /// The identity of the document the run executes.
    fn document(&self) -> DocumentId;

    /// The bounds the run is held to.
    fn bounds(&self) -> Bounds;

    /// The helper releases this build holds: a park pins the manifest its
    /// root names.
    ///
    /// # Errors
    ///
    /// [`KernelFailure::State`] when the build's releases do not stage.
    fn releases(&self) -> Result<&ReleaseGraph, KernelFailure>;

    /// A machine that has executed nothing, whose every root is bound to
    /// the OwnerBinding node `binding`, its pages read and sealed through
    /// `pages`. It may refuse its start at its first `run`.
    async fn start(
        &self,
        binding: NodeId,
        pages: SharedPages,
    ) -> Result<Self::Machine, KernelFailure>;

    /// A machine opened on the run's committed checkpoint root, read
    /// through `pages` under the activation's lease on it. It may refuse
    /// the root at its first `run`.
    async fn resume(
        &self,
        parked: &ParkedState,
        binding: NodeId,
        pages: SharedPages,
    ) -> Result<Self::Machine, KernelFailure>;
}

/// How a run's activations read and stage pages (`D-SPOOL`, `D-LEASE`,
/// `D-CAP`): what the host decides and the broker cannot.
#[derive(Clone)]
pub struct PagePlan {
    /// Opens the spool an activation stages into: its own, or the one its
    /// session's turn shares ([`super::pages::SharedSpool`]).
    pub spool: Arc<dyn Fn() -> Result<Box<dyn StageSpool>, StageFault> + Send + Sync>,
    /// The lease a fresh run reads under: its carried session's root. None
    /// for a run that reads only what it stages.
    pub fresh: Option<RootLease>,
    /// Pages a fresh run may name without reading them: its carried
    /// session's roots, which its lease protects.
    pub references: Vec<NodeId>,
    pub limits: PageLimits,
    /// How long a lease the broker takes on an execution's root lives
    /// between renewals; the page service renews it at a third.
    pub lease_ttl: Duration,
    /// How long all of one activation's page exchanges may take.
    pub span: Duration,
    /// The committed pages the process read, which an activation's reads
    /// answer from before the store (FIG-6321).
    pub cache: Option<super::pages::cache::PageCache>,
    /// What the session's last activation touched, which the next one's
    /// start carries warm (FIG-6321).
    pub hints: Option<super::pages::cache::PageHints>,
}

impl PagePlan {
    /// A plan over spools `spools` opens, for a run that carries no
    /// session: a 60 s lease and a one-hour activation span.
    #[must_use]
    pub fn over(spools: Arc<SpoolRoot>, limits: PageLimits) -> Self {
        Self {
            spool: Arc::new(move || {
                spools
                    .spool(next_stage_lease())
                    .map(|spool| Box::new(spool) as Box<dyn StageSpool>)
            }),
            fresh: None,
            references: Vec::new(),
            limits,
            lease_ttl: Duration::from_secs(60),
            span: Duration::from_secs(3600),
            cache: None,
            hints: None,
        }
    }
}

/// The kernel versions this build's machines open a checkpoint root of.
#[must_use]
pub fn kernel_reads() -> lash_sansio::VersionRange {
    let newest = lash_kernel_doc::KernelVersion::NEWEST.number();
    let oldest = lash_kernel_doc::KernelVersion::ALL
        .iter()
        .map(|version| version.number())
        .min()
        .unwrap_or(newest);
    lash_sansio::VersionRange::between(oldest, newest)
}

/// A process-unique staging lease, and with it a capability epoch: no two
/// activations of one parent share either.
#[must_use]
pub fn next_stage_lease() -> StageLeaseId {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    StageLeaseId(NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The most a run's bounds may allow: the broker's own ceilings on what one
/// park admits and one list `join` holds, enforced here as well as in the
/// machine (`K-BND-001`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelCeilings {
    pub requests_per_park: u32,
    pub join_members: u32,
}

impl KernelCeilings {
    /// Refuses `bounds` that allow more than the broker does.
    ///
    /// # Errors
    ///
    /// [`KernelFailure::OverCeiling`], naming the bound.
    pub fn check(&self, bounds: &Bounds) -> Result<(), KernelFailure> {
        for (bound, stated, ceiling) in [
            (
                Bound::RequestsPerPark,
                bounds.requests_per_park,
                self.requests_per_park,
            ),
            (Bound::JoinMembers, bounds.join_members, self.join_members),
        ] {
            if stated > ceiling {
                return Err(KernelFailure::OverCeiling {
                    bound,
                    stated,
                    ceiling,
                });
            }
        }
        Ok(())
    }
}

/// The bound a park of `requests` waits passes, if it passes one: the
/// broker admits none of them, and the run ends there (`K-BND-001`).
pub fn park_bound(bounds: &Bounds, requests: usize) -> Option<BoundExceeded> {
    (requests > bounds.requests_per_park as usize).then(|| BoundExceeded {
        bound: Bound::RequestsPerPark,
        limit: u64::from(bounds.requests_per_park),
        function: None,
    })
}

/// How a driven run stopped.
pub enum KernelEnd {
    /// The run ended. `owed` is the ledger its end commits over when the
    /// run has a parked root (see [`DurableSnapshotStore::commit_run_end`]):
    /// none for an end already recorded, a cancelled run, or one that never
    /// parked, whose stretch admitted nothing and runs again from its
    /// start. `pages` is the last activation's page service, whose spool
    /// holds what the end carries and whose lease reads what the run
    /// committed, until the end's consumer ends it ([`end_activation`]);
    /// none for an end read back from its record.
    Ended {
        end: End,
        owed: Option<Box<EffectLedger>>,
        pages: Option<SharedPages>,
    },
    /// The run is parked on waits that outlive this activation; its
    /// checkpoint resumes it.
    Suspended,
}

impl std::fmt::Debug for KernelEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ended { end, owed, .. } => f
                .debug_struct("Ended")
                .field("end", end)
                .field("owed", owed)
                .finish_non_exhaustive(),
            Self::Suspended => f.write_str("Suspended"),
        }
    }
}

/// Why a run did not stop where [`KernelEnd`] says. Every variant is typed.
#[derive(Debug, thiserror::Error)]
pub enum KernelFailure {
    #[error("the run's {bound:?} bound of {stated} is over the broker's ceiling of {ceiling}")]
    OverCeiling {
        bound: Bound,
        stated: u32,
        ceiling: u32,
    },
    #[error("the document has no identity: {0}")]
    Document(#[from] EncodeError),
    #[error(transparent)]
    Start(#[from] StartError),
    #[error("the run's checkpoint root is refused: {0}")]
    Open(#[from] OpenError),
    /// The root the machine sealed fails the parent's header check.
    #[error("the sealed checkpoint root is refused: {0}")]
    Sealed(#[from] CheckpointRefusal),
    /// The activation's staging failed: a host outcome; the run's last
    /// checkpoint stands.
    #[error("the run's staging failed: {0}")]
    Staging(#[from] StageFault),
    /// The store refused the activation's root lease.
    #[error("the run's root lease was refused: {0}")]
    Lease(DurableError),
    #[error("the run's graph state was refused: {0}")]
    State(GraphStateFailure),
    #[error("the saved checkpoint records neither a state nor an end")]
    EmptyCheckpoint,
    #[error(transparent)]
    Machine(#[from] MachineError),
    #[error("the machine refused a committed outcome: {0}")]
    Deliver(#[from] DeliverError),
    #[error("the machine parked on a wait the broker does not hold")]
    Stalled,
    #[error(transparent)]
    Checkpoint(#[from] QuietPointRefusal),
    #[error(transparent)]
    Parent(#[from] ParentFault),
    /// The worker that hosted the machine failed, refused the run or could
    /// not be had: nothing about the guest. The run's last checkpoint
    /// stands.
    #[error("the run's worker failed: {outcome}")]
    Worker { outcome: InfrastructureOutcome },
}

/// One owner's broker for kernel runs.
pub struct KernelBroker<'a> {
    pub store: &'a DurableSnapshotStore,
    pub effects: &'a dyn KernelEffects,
    /// The one derivation of the run's call identities (ADR 0117).
    pub identities: &'a CodeCallIdentities,
    pub ceilings: KernelCeilings,
    /// The charge units one [`Machine::run`] call may spend before it
    /// returns to the broker.
    pub slice: u64,
    /// How the run's activations read and stage pages.
    pub pages: &'a PagePlan,
}

/// One activation's page service, holding the lease the broker took for
/// it until the activation ends ([`end_activation`]).
pub struct PageActivation {
    pub pages: SharedPages,
}

/// Ends `pages`' activation: the lease on the execution's root its broker
/// took goes (its expiry would end it as well); the host's own stays.
pub async fn end_activation(
    store: &dyn lash_durable::domain::pages::PageStore,
    pages: &SharedPages,
) {
    let lease = pages.lock().await.end();
    if let Some(lease) = lease {
        let _ = store.release_root_lease(lease.id).await;
    }
}

impl KernelBroker<'_> {
    /// Drives the run `machines` starts to its end, or to a park that
    /// outlives this activation, resuming from the run's checkpoint when it
    /// has one. `cancel` cancels the run and its open effects: the machine
    /// observes it at its next safe point.
    ///
    /// # Errors
    ///
    /// [`KernelFailure`]. The run's last checkpoint stands.
    #[expect(clippy::too_many_lines, reason = "one run's drive, in its order")]
    pub async fn run<S: Machines>(
        &self,
        machines: &S,
        cancel: &CancellationToken,
    ) -> Result<KernelEnd, KernelFailure> {
        let bounds = machines.bounds();
        self.ceilings.check(&bounds)?;
        let document = machines.document();
        let binding = super::graph::owner_binding_node(self.store.exec())
            .map_err(|refusal| KernelFailure::State(refusal.into()))?;
        // A run with a checkpoint commits its end over it; one that never
        // parked commits nothing, and runs again from its start.
        let mut saved = false;
        // The saved state the machine stands at exactly, while it does: a
        // machine that only waits gives its worker back and is reopened
        // from this.
        let mut resting: Option<ParkedState> = None;
        let checkpoint = self.store.checkpoint(&document, self.effects).await?;
        // A held response rides only a fresh run's first save, within its
        // first slice: a run that saved already commits it on its own.
        let mut held = self.effects.held_response();
        if checkpoint.is_some() {
            self.commit_response_alone(&mut held).await?;
        }
        let (mut activation, machine, mut ledger) = match checkpoint {
            Some(Checkpoint::Ended { end, .. }) => {
                return Ok(KernelEnd::Ended {
                    end,
                    owed: None,
                    pages: None,
                });
            }
            Some(Checkpoint::Parked { state, ledger, .. }) => {
                saved = true;
                self.effects.resumed(&self.store.open_requests(&ledger))?;
                let activation = self.activation(Some(&state), binding, machines).await?;
                let machine = machines
                    .resume(&state, binding, activation.pages.clone())
                    .await?;
                (activation, machine, ledger)
            }
            None => {
                let activation = self.activation(None, binding, machines).await?;
                let machine = machines.start(binding, activation.pages.clone()).await?;
                (activation, machine, EffectLedger::new())
            }
        };
        let mut machine = Some(machine);
        // A fresh run that reads saves its start first, before any slice
        // runs, with the response the turn holds: that save owes the
        // providers it reads through before any read is made, and a crash
        // after it runs the cell again from its start.
        if !saved
            && self.effects.plans_reads()
            && let Some(running) = machine.as_mut()
        {
            let state = self
                .sealed(running, &activation, binding, &document)
                .await?;
            let save = ParkSave {
                document,
                state: state.clone(),
                pages: &activation.pages,
                releases: machines.releases()?,
                ledger,
                admit: Vec::new(),
                with: Vec::new(),
            };
            let committed = match self.commit_save(save, &mut held).await? {
                Ok(committed) => committed,
                Err(response) => {
                    if let Some(stopped) = machine.take() {
                        stopped.release().await?;
                    }
                    self.end_activation(activation).await;
                    response.stop();
                    return std::future::pending().await;
                }
            };
            saved = true;
            ledger = committed.ledger;
            self.lease_published(&activation, committed.root).await?;
            resting = Some(self.resting(committed.root, state, &ledger)?);
        }
        loop {
            let running = match &mut machine {
                Some(running) => running,
                None => {
                    let state = resting.take().ok_or(KernelFailure::EmptyCheckpoint)?;
                    let previous = std::mem::replace(
                        &mut activation,
                        self.activation(Some(&state), binding, machines).await?,
                    );
                    self.end_activation(previous).await;
                    let reopened = machines
                        .resume(&state, binding, activation.pages.clone())
                        .await?;
                    resting = Some(state);
                    machine.insert(reopened)
                }
            };
            let park = match running.run(self.slice, cancel.is_cancelled()).await? {
                // The first stretch outlasts its slice: the response the
                // turn held commits now, and the run goes on.
                MachineStep::Slice => {
                    self.commit_response_alone(&mut held).await?;
                    continue;
                }
                MachineStep::Ended(end) => {
                    if let Some(ended) = machine.take() {
                        ended.release().await?;
                    }
                    return Ok(self.ended(ledger, end, saved, activation).await);
                }
                MachineStep::Parked(park) => park,
            };
            let requests: Vec<Request> = park
                .requests
                .into_iter()
                .filter(|request| !park.withdrawn.contains(&wait_of(request)))
                .collect();
            let outstanding = park.outstanding;
            let live = park.live;
            for wait in park.withdrawn {
                ledger.withdraw(wait);
            }
            if let Some(exceeded) = park_bound(&bounds, requests.len()) {
                if let Some(refused) = machine.take() {
                    refused.release().await?;
                }
                let end = End::Error(RunError::Bound(exceeded));
                return Ok(self.ended(ledger, end, saved, activation).await);
            }
            if !requests.is_empty() {
                let admit = self
                    .admissions(&ledger, requests, outstanding, live)
                    .await?;
                let state = self
                    .sealed(running, &activation, binding, &document)
                    .await?;
                let save = ParkSave {
                    document,
                    state: state.clone(),
                    pages: &activation.pages,
                    releases: machines.releases()?,
                    ledger,
                    admit,
                    with: Vec::new(),
                };
                let committed = match self.commit_save(save, &mut held).await? {
                    Ok(committed) => committed,
                    // The turn's cancel stopped the cell at its first save:
                    // nothing it admitted starts, and the turn, which drops
                    // this run, stops where it would have before the cell
                    // started.
                    Err(response) => {
                        if let Some(stopped) = machine.take() {
                            stopped.release().await?;
                        }
                        self.end_activation(activation).await;
                        response.stop();
                        return std::future::pending().await;
                    }
                };
                saved = true;
                ledger = committed.ledger;
                self.lease_published(&activation, committed.root).await?;
                resting = Some(self.resting(committed.root, state, &ledger)?);
            }
            if ledger.pending().next().is_none() {
                return Err(KernelFailure::Stalled);
            }
            // The run waits on its effects: nothing is held across a wait.
            self.commit_response_alone(&mut held).await?;
            // Once the run waits only on parked rows, with its state saved
            // as it stands, its worker is another run's to use: a process
            // this run awaits may need it (FIG-4275).
            let quiet = tokio::sync::Notify::new();
            let driven = {
                let outcome = |effect: &AdmittedEffect, output: &SettledOutput| {
                    self.effects.outcome(effect, output)
                };
                let drive = self
                    .store
                    .drive_effects_as(&ledger, cancel, &outcome, Some(&quiet));
                tokio::pin!(drive);
                loop {
                    tokio::select! {
                        driven = &mut drive => break driven?,
                        () = quiet.notified(), if machine.is_some() && resting.is_some() => {
                            if let Some(waiting) = machine.take() {
                                waiting.release().await?;
                            }
                        }
                    }
                }
            };
            let settled = match driven {
                Driven::Answered(settled) => settled,
                Driven::Suspended => {
                    if let Some(parked) = machine.take() {
                        parked.release().await?;
                    }
                    self.end_activation(activation).await;
                    return Ok(KernelEnd::Suspended);
                }
            };
            if self.effects.stop_requested() {
                if let Some(stopped) = machine.take() {
                    stopped.release().await?;
                }
                return Ok(self.ended(ledger, End::Cancelled, saved, activation).await);
            }
            // A machine that gave its worker back is reopened from its
            // saved root, and reads these outcomes again once it has parked.
            let Some(running) = machine.as_mut() else {
                continue;
            };
            resting = None;
            for outcome in settled {
                running.deliver(outcome.wait, outcome.outcome).await?;
                ledger.consume(&outcome.identity);
            }
        }
    }

    /// The page service of one activation: leased on the run's committed
    /// root when it resumes from `parked`, on the host's fresh lease
    /// otherwise. Sealed pages may name the run's owner binding and the
    /// release manifests its roots pin.
    ///
    /// # Errors
    ///
    /// [`KernelFailure::Lease`] when the root cannot be leased, and the
    /// spool's faults.
    pub async fn activation<S: Machines>(
        &self,
        parked: Option<&ParkedState>,
        binding: NodeId,
        machines: &S,
    ) -> Result<PageActivation, KernelFailure> {
        let store = self.store.context().pages().map_err(KernelFailure::Lease)?;
        let (lease, owned) = match parked {
            Some(parked) => (Some(self.lease(&store, parked.exec_root).await?), true),
            None => (self.pages.fresh.clone(), false),
        };
        let releases = &machines.releases()?.manifests;
        let mut references: Vec<NodeId> = releases
            .dialects()
            .filter_map(|dialect| releases.current(dialect))
            .map(|release| NodeId(release.0))
            .collect();
        references.push(binding);
        match parked {
            Some(parked) => references.push(NodeId(parked.header.release.0)),
            None => references.extend(self.pages.references.iter().copied()),
        }
        let stage = next_stage_lease();
        let pages = ActivationPages::new(
            store,
            lease,
            self.pages.lease_ttl / 3,
            CapabilityKey::fresh(),
            stage.0,
            (self.pages.spool)()?,
            Instant::now() + self.pages.span,
            self.pages.limits,
        )
        .with_references(references);
        let pages = match &self.pages.cache {
            Some(cache) => pages.caching(cache.clone()),
            None => pages,
        };
        let pages = match &self.pages.hints {
            Some(hints) => pages.hinted(hints.clone()),
            None => pages,
        };
        let pages = if owned { pages.owning() } else { pages };
        Ok(PageActivation {
            pages: Arc::new(tokio::sync::Mutex::new(pages)),
        })
    }

    /// A lease on the execution root `root` under the actor's fence.
    async fn lease(
        &self,
        store: &Arc<dyn lash_durable::domain::pages::PageStore>,
        root: NodeId,
    ) -> Result<RootLease, KernelFailure> {
        let cx = self.store.context();
        store
            .acquire_root_lease(RootLeaseRequest {
                owner: RootOwner::Exec(self.store.exec().clone()),
                expected: root,
                actor: cx.actor().clone(),
                epoch: cx.epoch(),
                ttl_millis: i64::try_from(self.pages.lease_ttl.as_millis()).unwrap_or(i64::MAX),
            })
            .await
            .map_err(KernelFailure::Lease)
    }

    /// After a park published `root`, an activation that read under no
    /// lease of its own takes one on it: the pages its candidate staged
    /// are the store's now, and read there.
    async fn lease_published(
        &self,
        activation: &PageActivation,
        root: NodeId,
    ) -> Result<(), KernelFailure> {
        let mut pages = activation.pages.lock().await;
        if pages.lease().is_some() {
            return Ok(());
        }
        let store = self.store.context().pages().map_err(KernelFailure::Lease)?;
        let lease = self.lease(&store, root).await?;
        pages.lease_on(lease);
        Ok(())
    }

    /// Ends an activation's page service: the lease this broker took goes;
    /// its expiry would end it as well.
    pub async fn end_activation(&self, activation: PageActivation) {
        if let Ok(store) = self.store.context().pages() {
            end_activation(store.as_ref(), &activation.pages).await;
        }
    }

    /// The root `running` seals at this safe point, its header read from
    /// the activation's spool and checked: this kernel, this document,
    /// bound to this execution, pinning a release this build holds.
    ///
    /// # Errors
    ///
    /// [`KernelFailure::Sealed`] for a header that fails the check, and
    /// the machine's and the spool's failures.
    pub async fn sealed<M: DrivenMachine>(
        &self,
        running: &mut M,
        activation: &PageActivation,
        binding: NodeId,
        document: &DocumentId,
    ) -> Result<SealedPark, KernelFailure> {
        sealed(running, &activation.pages, binding, document).await
    }

    /// The state a committed park rests at: its root, under the execution
    /// root `published`, and the admissions of every wait it stands on.
    fn resting(
        &self,
        published: NodeId,
        state: SealedPark,
        ledger: &EffectLedger,
    ) -> Result<ParkedState, KernelFailure> {
        Ok(ParkedState {
            exec_root: published,
            root: state.root,
            header: state.header,
            admissions: self.store.admitted_waits(ledger)?,
        })
    }

    /// Commits `save`, with the response the turn still holds for the run
    /// when it holds one. `Err` is that response, when the turn's cancel
    /// stopped the cell at this, its first save.
    async fn commit_save<'h>(
        &self,
        save: ParkSave<'_>,
        held: &mut Option<&'h lash_core_execution::HeldResponse>,
    ) -> Result<Result<super::store::Saved, &'h lash_core_execution::HeldResponse>, KernelFailure>
    {
        Ok(match held.take() {
            Some(response) => match self
                .store
                .commit_first_park(save, self.effects, response)
                .await?
            {
                FirstPark::Saved(saved) => Ok(saved),
                FirstPark::Stopped => Err(response),
            },
            None => Ok(self.store.commit_park(save, self.effects).await?),
        })
    }

    /// Commit the response the turn holds for this run on its own, as
    /// `model.done`, when it is still held: the run is about to save
    /// without it, or to run past its first slice.
    async fn commit_response_alone(
        &self,
        held: &mut Option<&lash_core_execution::HeldResponse>,
    ) -> Result<(), KernelFailure> {
        if let Some(response) = held.take() {
            response
                .commit_alone(self.store.context())
                .await
                .map_err(|error| QuietPointRefusal(error.to_string()))?;
        }
        Ok(())
    }

    /// How each of a park's requests is admitted: an effect as the
    /// execution the parent drafts for it under the call the park and its
    /// place there derive, a sleep with its deadline on the store's clock.
    async fn admissions(
        &self,
        ledger: &EffectLedger,
        requests: Vec<Request>,
        outstanding: u64,
        live: u64,
    ) -> Result<Vec<EffectAdmission>, KernelFailure> {
        let park = ledger.next_park();
        let mut now = None::<DurableInstant>;
        let mut admit = Vec::with_capacity(requests.len());
        let mut executions = 0_u64;
        for request in requests {
            admit.push(match request {
                Request::Effect(request) => {
                    let call = self.identities.child_call_id(park, executions);
                    let how = match self
                        .effects
                        .admit(&request, call, outstanding, live)
                        .await?
                    {
                        Ok(draft) => {
                            executions += 1;
                            AdmitAs::Execution(Box::new(draft))
                        }
                        Err(error) => AdmitAs::Refused {
                            error,
                            args: request.args.clone(),
                        },
                    };
                    EffectAdmission {
                        identity: request.identity,
                        wait: request.wait,
                        effect: Some(request.effect),
                        admit: how,
                    }
                }
                Request::Sleep(sleep) => {
                    let now = match now {
                        Some(now) => now,
                        None => *now.insert(
                            self.store
                                .context()
                                .durable_now()
                                .await
                                .map_err(|error| QuietPointRefusal(error.to_string()))?,
                        ),
                    };
                    let millis = i64::try_from(sleep.duration.as_millis()).unwrap_or(i64::MAX);
                    EffectAdmission {
                        identity: sleep.identity,
                        wait: sleep.wait,
                        effect: None,
                        admit: AdmitAs::Sleep {
                            duration: sleep.duration,
                            until: DurableInstant(now.0.saturating_add(millis)),
                        },
                    }
                }
            });
        }
        Ok(admit)
    }

    /// How a run that ended stops: its end owed over its parked root, or
    /// nothing owed for a cancelled run, which records no end, or one that
    /// never parked, whose stretch admitted nothing and runs again from its
    /// start. The activation stays: its consumer reads what the run
    /// committed, then ends it ([`end_activation`]).
    async fn ended(
        &self,
        ledger: EffectLedger,
        end: End,
        saved: bool,
        activation: PageActivation,
    ) -> KernelEnd {
        let owed = (saved && !matches!(end, End::Cancelled)).then(|| Box::new(ledger));
        let pages = activation.pages;
        KernelEnd::Ended {
            end,
            owed,
            pages: Some(pages),
        }
    }
}

fn wait_of(request: &Request) -> WaitId {
    match request {
        Request::Effect(request) => request.wait,
        Request::Sleep(sleep) => sleep.wait,
    }
}

/// The root `running` seals at this safe point, its header read from the
/// activation's spool through `pages` and checked: this kernel, this
/// document, bound to the OwnerBinding node `binding`, pinning a release
/// this build holds.
///
/// # Errors
///
/// [`KernelFailure::Sealed`] for a header that fails the check, and the
/// machine's and the spool's failures.
pub async fn sealed<M: DrivenMachine>(
    running: &mut M,
    pages: &SharedPages,
    binding: NodeId,
    document: &DocumentId,
) -> Result<SealedPark, KernelFailure> {
    let root = running.seal().await?;
    let bytes = pages.lock().await.spool().read(&root)?;
    let header = checkpoint_header(root, &bytes).map_err(KernelFailure::State)?;
    let reads = kernel_reads();
    if !reads.contains(header.kernel) {
        return Err(CheckpointRefusal::KernelOutsideReadRange {
            found: header.kernel,
            reads,
        }
        .into());
    }
    if header.document != *document {
        return Err(CheckpointRefusal::WrongDocument {
            expected: document.to_string(),
            found: header.document.to_string(),
        }
        .into());
    }
    if header.owner.0 != binding.0 {
        return Err(CheckpointRefusal::WrongOwner {
            expected: binding.to_string(),
            found: NodeId(header.owner.0).to_string(),
        }
        .into());
    }
    Ok(SealedPark { root, header })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(requests_per_park: u32, join_members: u32) -> Bounds {
        Bounds {
            charge: u64::MAX,
            memory: u64::MAX,
            call_depth: 64,
            live_tasks: 8,
            requests_per_park,
            join_members,
        }
    }

    /// `K-BND-001` at the broker: a run whose bounds allow more than the
    /// broker's ceilings does not start, and a park over the run's bound
    /// admits nothing and ends the run with the typed bound error.
    #[test]
    fn the_broker_holds_a_run_to_its_park_and_join_bounds() {
        let ceilings = KernelCeilings {
            requests_per_park: 4,
            join_members: 8,
        };
        assert!(ceilings.check(&bounds(4, 8)).is_ok());
        assert!(matches!(
            ceilings.check(&bounds(5, 8)),
            Err(KernelFailure::OverCeiling {
                bound: Bound::RequestsPerPark,
                stated: 5,
                ceiling: 4,
            })
        ));
        assert!(matches!(
            ceilings.check(&bounds(4, 9)),
            Err(KernelFailure::OverCeiling {
                bound: Bound::JoinMembers,
                stated: 9,
                ceiling: 8,
            })
        ));
        assert_eq!(park_bound(&bounds(4, 8), 4), None);
        assert_eq!(
            park_bound(&bounds(4, 8), 5),
            Some(BoundExceeded {
                bound: Bound::RequestsPerPark,
                limit: 4,
                function: None,
            })
        );
    }
}
