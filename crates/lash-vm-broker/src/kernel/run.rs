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
use std::time::Duration;

use lash_core_store::checkpoint_graph::stage::{StageFault, StageLeaseId, StageSpool};
use lash_core_store::checkpoint_graph::{GraphStaging, NodeId};
use lash_durable::DurableError;
use lash_durable::domain::graph::RootOwner;
use lash_durable::domain::pages::{RootLease, RootLeaseRequest};
use lash_kernel_doc::ErrorDatum;
use lash_kernel_value::names::DocumentId;
use lash_kernel_vm::{
    Bound, BoundExceeded, Bounds, DeliverError, EffectRequest, End, MachineError, OpenError,
    Outcome, Request, RunError, StartError, WaitId,
};
use lash_protocol_types::engine_fault::{CheckpointRefusal, InfrastructureOutcome};
use tokio_util::sync::CancellationToken;

use super::closure::Closure;
use super::graph::FrozenRequest as RecordedRequest;
use super::graph::{FrozenRequest, Projections};
use super::ledger::{AdmittedEffect, EffectLedger, Growth};
use super::ledger::{PendingEffect, Standing};
use super::pages::{
    ActivationPages, CapabilityKey, PageLimits, SharedPages, SpoolRoot, renew_when_due,
};
use super::state::{GraphStateFailure, checkpoint_header};
use super::store::{
    AdmitAs, AdmittedCall, CellJournal, Checkpoint, EffectAdmission, HostParts, ParkSave,
    ParkedState, SealedPark,
};
use crate::effects::{MemberDraft, ParentFault};
use crate::identity::CodeCallIdentities;
use crate::members::Driven;
use crate::snapshot::{BrokerInvariant, DriveRefusal, DurableSnapshotStore, QuietPointRefusal};
use lash_core_store::tool_run::SettledOutput;
use lash_durable::domain::{DomainWrite, EffectAdmissionWrite};
use lash_kernel_value::sites::EffectId;

/// The declared reads a commit records: each read's caller-owned call, its
/// ordinal and its fact's node.
pub type ReadCalls = Vec<(
    lash_durable::domain::CallOwner,
    lash_protocol_types::ToolCallId,
    u64,
    NodeId,
)>;

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
        call: lash_protocol_types::ToolCallId,
        outstanding: u64,
        live: u64,
    ) -> Result<Result<MemberDraft, ErrorDatum>, ParentFault>;

    /// A run that follows its journal reached `request`, which its journal
    /// admitted as `recorded`: the host takes the call up as it stands,
    /// without judging it again (FIG-7611). The default keeps nothing.
    ///
    /// # Errors
    ///
    /// [`ParentFault`] when the host cannot take the call up.
    async fn readmit(
        &self,
        request: &EffectRequest,
        recorded: &FrozenRequest,
    ) -> Result<(), ParentFault> {
        let _ = (request, recorded);
        Ok(())
    }

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
    /// names, awaited or cancelling, with the request its admission froze.
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

    /// Stage immutable inputs needed to reconstruct this run without a
    /// heap checkpoint. The admission transaction retains the returned node.
    fn stage_context(&self, _staging: &mut GraphStaging) -> Result<Option<NodeId>, ParentFault> {
        Ok(None)
    }

    /// The save that staged the host's state committed.
    fn host_committed(&self) {}

    /// The run's journal holds these admissions, each call with its
    /// Admission node: a commit admitted them, or the run follows a
    /// journal that did (FIG-7585). The host's next checkpoint records
    /// each call against its admission.
    fn journaled(&self, admissions: &[(lash_protocol_types::ToolCallId, NodeId)]) {
        let _ = admissions;
    }

    /// The declared reads an in-turn run made since its last commit, each
    /// staged as its read fact: the caller-owned call, its ordinal and the
    /// fact's node. Its next commit records them, so a run that resumes
    /// from its start is answered the same reads (FIG-7585).
    ///
    /// # Errors
    ///
    /// [`ParentFault`] when a fact does not stage.
    fn stage_reads(&self, staging: &mut GraphStaging) -> Result<ReadCalls, ParentFault> {
        let _ = staging;
        Ok(Vec::new())
    }

    /// What an in-turn run owes a node that runs it, from the committed
    /// records it runs under: the callbacks and the providers and codecs of
    /// its environment and its execution's resolved matches (FIG-7585).
    fn debts(
        &self,
    ) -> (
        Vec<lash_vocabulary::providers::PluginCallbackIdentity>,
        lash_vocabulary::claims::ClaimRequirements,
    ) {
        (
            Vec::new(),
            lash_vocabulary::claims::ClaimRequirements::default(),
        )
    }

    /// How many values the run printed so far: a park's journal entry
    /// records it, and a run that follows its journal must have printed as
    /// many at that park (FIG-7585).
    fn prints(&self) -> u64 {
        0
    }

    /// The run starts again from its last checkpoint on this activation:
    /// the host's record of its effects, reads and prints goes back to what
    /// it held at that checkpoint ([`mark`](Self::mark)), and the run
    /// records the rest again as it follows its journal (FIG-7585).
    fn restart(&self) {}

    /// The run's state at this point is its checkpoint: what the host
    /// holds of its effects, reads and prints is what a
    /// [`restart`](Self::restart) goes back to (FIG-7585).
    fn mark(&self) {}

    /// The model response that asked for this run, while its turn holds it
    /// uncommitted for the run's first save (FIG-6000): that save commits it
    /// in the same transaction. The response commits on its own once the
    /// run's first stretch outlasts one slice.
    fn held_response(&self) -> Option<&lash_core_execution::HeldResponse> {
        None
    }
}

pub use lash_core_execution::kernel_runner::{MachinePark, MachineStep, park_bound};

/// One run's machine, wherever it executes. Between calls the run is at a
/// safe point. Its heap reads and seals pages through the activation's
/// page service ([`SharedPages`]), which the broker publishes from.
#[async_trait::async_trait]
pub trait DrivenMachine: Send {
    /// Latest received kernel receipt; missing means unknown, never zero.
    fn meters(&self) -> Option<lash_kernel_vm::Meters> {
        None
    }

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

    /// The execution closure of the run's document, over the functions
    /// this host links documents against: what each root of the run names,
    /// and what its first park stages.
    ///
    /// # Errors
    ///
    /// [`KernelFailure::State`] when this host does not hold a function the
    /// document reaches.
    fn closure(&self) -> Result<&Closure, KernelFailure>;

    /// The execution closures of the session's earlier cells that no
    /// commit installed yet: a park whose pages name one, through a
    /// function its cell saved, stages it.
    fn carried(&self) -> &[Closure] {
        &[]
    }

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
pub fn kernel_reads() -> lash_protocol_types::VersionRange {
    let newest = lash_kernel_doc::KernelVersion::NEWEST.number();
    let oldest = lash_kernel_doc::KernelVersion::ALL
        .iter()
        .map(|version| version.number())
        .min()
        .unwrap_or(newest);
    lash_protocol_types::VersionRange::between(oldest, newest)
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
/// machine (`K-BND-001`), and on what the run's parks append over its whole
/// life, which only the broker counts (FIG-6788).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelCeilings {
    pub requests_per_park: u32,
    pub join_members: u32,
    /// The most effects and sleeps the run's parks may admit.
    pub effects: u64,
    /// The most page bytes the run may seal into its checkpoints.
    pub checkpoint_bytes: u64,
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

    /// The run's growth once a save appends `effects` admissions and
    /// `sealed` page bytes to `growth`, or the lifetime bound it passes:
    /// that save admits nothing, and the run ends there (FIG-6788).
    ///
    /// # Errors
    ///
    /// [`Bound::Effects`] or [`Bound::CheckpointBytes`], the first passed.
    pub fn grow(&self, growth: Growth, effects: u64, sealed: u64) -> Result<Growth, BoundExceeded> {
        let grown = Growth {
            effects: growth.effects.saturating_add(effects),
            checkpoint_bytes: growth.checkpoint_bytes.saturating_add(sealed),
        };
        let passed = |bound, limit| BoundExceeded {
            bound,
            limit,
            function: None,
            site: None,
        };
        if grown.effects > self.effects {
            return Err(passed(Bound::Effects, self.effects));
        }
        if grown.checkpoint_bytes > self.checkpoint_bytes {
            return Err(passed(Bound::CheckpointBytes, self.checkpoint_bytes));
        }
        Ok(grown)
    }
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
    /// The owner's read or commit failed, retaining its fence cause.
    #[error(transparent)]
    Durable(#[from] DurableError),
    #[error("the run's {bound:?} bound of {stated} is over the broker's ceiling of {ceiling}")]
    OverCeiling {
        bound: Bound,
        stated: u32,
        ceiling: u32,
    },
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
    /// An in-turn run that resumed from its start did not follow its
    /// journal: it requested another effect, other arguments or another
    /// order than the run its commits recorded, or runs another document
    /// or closure. Nothing it requested was admitted (FIG-7585).
    #[error("the run's replay diverged from its journal: {0}")]
    Diverged(String),
    #[error(transparent)]
    Checkpoint(QuietPointRefusal),
    /// The broker's own invariant failed, not the store: the run ends with
    /// it, since every pass would meet it again.
    #[error(transparent)]
    Invariant(#[from] BrokerInvariant),
    #[error(transparent)]
    Parent(#[from] ParentFault),
    /// The worker that hosted the machine failed, refused the run or could
    /// not be had: nothing about the guest. The run's last checkpoint
    /// stands.
    #[error("the run's worker failed: {outcome}")]
    Worker { outcome: InfrastructureOutcome },
}

impl From<QuietPointRefusal> for KernelFailure {
    fn from(refusal: QuietPointRefusal) -> Self {
        match refusal {
            QuietPointRefusal::Durable(error) => Self::Durable(error),
            QuietPointRefusal::Refused(_) => Self::Checkpoint(refusal),
        }
    }
}

impl From<DriveRefusal> for KernelFailure {
    fn from(refusal: DriveRefusal) -> Self {
        match refusal {
            DriveRefusal::Durable(error) => Self::Durable(error),
            DriveRefusal::Refused(refusal) => refusal.into(),
            DriveRefusal::Invariant(invariant) => Self::Invariant(invariant),
        }
    }
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

/// Ends `pages`' activation: the leases on the execution's roots its
/// broker took go (their expiry would end them as well); the host's own
/// stays.
pub async fn end_activation(
    store: &dyn lash_durable::domain::pages::PageStore,
    pages: &SharedPages,
) {
    let leases = pages.lock().await.end();
    for lease in leases {
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
        let checkpoint = self.store.checkpoint(&document, self.effects).await?;
        // A held response rides only a fresh run's first save, within its
        // first slice: a run that saved already commits it on its own.
        let mut held = self.effects.held_response();
        if checkpoint.is_some() {
            self.commit_response_alone(&mut held).await?;
        }
        // A run saves its state only at a checkpoint (FIG-7585): it starts
        // from its last one, or from its start, and follows the journal its
        // commits recorded since.
        let base = match checkpoint {
            Some(Checkpoint::Ended { end, .. }) => {
                return Ok(KernelEnd::Ended {
                    end,
                    owed: None,
                    pages: None,
                });
            }
            Some(Checkpoint::Parked { state, ledger, .. }) => Some((state, ledger)),
            None => None,
        };
        let journal = self.store.journal(self.effects).await?;
        // A run follows only a journal its own document and closure wrote:
        // any other code is refused before it runs a step (FIG-7585).
        let fingerprint = super::graph::RunFingerprint {
            document,
            closure: machines.closure()?.reference(),
        };
        if journal
            .admitted
            .values()
            .any(|recorded| recorded.admission.run != fingerprint)
        {
            return Err(diverged(
                1,
                Field::Code,
                "it executes another document or closure than its journal recorded",
            ));
        }
        // A run follows records its earlier runs' commits left out of the
        // cache: it reads them from the store.
        self.store.reload_records().await;
        // The host knows every call the checkpoint stands on and the journal
        // admitted before any drive folds them: one that started and never
        // settled runs again (or is interrupted) as its own call, not as a
        // stranger.
        let mut admitted: Vec<(AdmittedEffect, FrozenRequest)> = base
            .as_ref()
            .map(|(_, ledger)| self.store.open_requests(ledger))
            .unwrap_or_default();
        admitted.extend(journal.admitted.values().filter_map(|resolved| {
            match &resolved.admission.disposition {
                super::graph::Disposition::Execution { operation, pin } => Some((
                    AdmittedEffect {
                        operation: *operation,
                        call: pin.call.clone(),
                    },
                    resolved.request.clone(),
                )),
                super::graph::Disposition::Refused => None,
            }
        }));
        self.effects.resumed(&admitted)?;
        self.effects.journaled(&journal_calls(&journal));
        self.effects.mark();
        let (activation, machine) = match &base {
            Some((state, _)) => {
                let activation = self.activation(Some(state), binding, machines).await?;
                let machine = machines
                    .resume(state, binding, activation.pages.clone())
                    .await?;
                (activation, machine)
            }
            None => {
                let activation = self.activation(None, binding, machines).await?;
                let machine = machines.start(binding, activation.pages.clone()).await?;
                (activation, machine)
            }
        };
        let mut owner = CellKernelOwner {
            broker: self,
            machines,
            cancel,
            activation,
            machine: Some(machine),
            ledger: base
                .as_ref()
                .map(|(_, ledger)| ledger.clone())
                .unwrap_or_default(),
            binding,
            document,
            saved: base.is_some() || !journal.admitted.is_empty(),
            held,
            journal: Journal::following(journal, base),
        };
        match lash_core_execution::kernel_runner::run(&mut owner).await? {
            CellStop::Ended(end) => Ok(self
                .ended(owner.ledger, end, owner.saved, owner.activation)
                .await),
            CellStop::Suspended => {
                self.end_activation(owner.activation).await;
                Ok(KernelEnd::Suspended)
            }
        }
    }

    /// The page service of one activation: leased on the run's committed
    /// root when it resumes from `parked`, on the host's fresh lease
    /// otherwise. Sealed pages may name the run's owner binding and the
    /// execution closure its roots pin.
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
        let mut references = vec![machines.closure()?.reference(), binding];
        if parked.is_none() {
            references.extend(self.pages.references.iter().copied());
        }
        let stage = next_stage_lease();
        let pages = ActivationPages::new(
            store,
            lease,
            self.pages.lease_ttl / 3,
            CapabilityKey::fresh(),
            stage.0,
            (self.pages.spool)()?,
            Arc::clone(self.store.context().clock()),
            self.pages.span,
            self.pages.limits,
        )
        .with_references(references)
        .with_lease_ttl(self.pages.lease_ttl);
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
                fence: lash_durable::domain::pages::LeaseFence::Actor {
                    actor: cx.actor().clone(),
                    epoch: cx.epoch(),
                },
                ttl_millis: i64::try_from(self.pages.lease_ttl.as_millis()).unwrap_or(i64::MAX),
            })
            .await
            .map_err(KernelFailure::Lease)
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
    /// bound to this execution, pinning its document's execution closure.
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
        closure: NodeId,
    ) -> Result<SealedPark, KernelFailure> {
        sealed(running, &activation.pages, binding, document, closure).await
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

    /// Commit the response the turn holds for this run on its own, as
    /// `model.done`, when it is still held: the run is about to save
    /// without it, or to run past its first slice.
    async fn commit_response_alone(
        &self,
        held: &mut Option<&lash_core_execution::HeldResponse>,
    ) -> Result<(), KernelFailure> {
        if let Some(response) = held.take() {
            response.commit_alone(self.store.context()).await?;
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
        progress: super::graph::ParkProgress,
    ) -> Result<Vec<EffectAdmission>, KernelFailure> {
        let park = ledger.next_park();
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
                        effect: Some(request.use_),
                        admit: how,
                        progress,
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

enum CellStop {
    Ended(End),
    Suspended,
}

struct CellKernelOwner<'a, 'b, S: Machines> {
    broker: &'a KernelBroker<'b>,
    machines: &'a S,
    cancel: &'a CancellationToken,
    activation: PageActivation,
    machine: Option<S::Machine>,
    ledger: EffectLedger,
    binding: NodeId,
    document: DocumentId,
    /// A run that admitted an effect or checkpointed commits its end over
    /// what it committed; one that did neither commits nothing, and runs
    /// again from its start.
    saved: bool,
    held: Option<&'a lash_core_execution::HeldResponse>,
    /// The run's last checkpoint and its journal since, and how far the
    /// machine follows it.
    journal: Journal,
}

/// The delivery batches a run's journal holds since its last checkpoint
/// before its next park takes a checkpoint (FIG-7585), besides the
/// checkpoint its owner's commit takes at its end. The bound is a resume
/// budget: a run re-runs only after a crash or after its worker was
/// released (no quiet release in 310 corpus parks, 2026-11-01), and then
/// re-runs every park since its last checkpoint at ~13.2M worker and host
/// instructions each (a 200-call TypeScript cell released at every park).
/// One second of CPU on the measuring host is ~8.1G instructions (the
/// parent's 200-call turn: 9.32G host instructions in 1155 ms), so 600
/// batches (~7.9G) keep the worst re-run within that second.
const CHECKPOINT_BATCHES: usize = 600;

/// A run's journal as one activation follows and extends it (FIG-7585):
/// its last checkpoint, the committed facts since, then what this
/// activation added, so a machine that gave its worker back starts again
/// from that checkpoint and follows all of it.
struct Journal {
    /// The run's last checkpoint: the state the machine resumes from, and
    /// the ledger it stood on there. None before the run's first.
    base: Option<(ParkedState, EffectLedger)>,
    journal: CellJournal,
    /// The batches the machine took so far.
    batch: usize,
    /// The admissions the machine requested again so far.
    matched: usize,
    /// The parks the machine made so far, from where it started: what a
    /// divergence names.
    parks: u64,
    /// The deliveries no commit recorded yet, in order.
    pending: Vec<(EffectId, lash_durable::domain::Delivery)>,
}

impl Journal {
    fn following(journal: CellJournal, base: Option<(ParkedState, EffectLedger)>) -> Self {
        Self {
            base,
            journal,
            batch: 0,
            matched: 0,
            parks: 0,
            pending: Vec::new(),
        }
    }

    /// Whether the machine has taken every recorded batch.
    fn delivered_all(&self) -> bool {
        self.batch >= self.journal.batches.len()
    }

    /// The machine starts again from the last checkpoint.
    fn restart(&mut self) {
        self.batch = 0;
        self.matched = 0;
        self.parks = 0;
    }

    /// Whether the run's next commit takes a checkpoint.
    fn due(&self) -> bool {
        self.journal.batches.len() >= CHECKPOINT_BATCHES
    }

    /// A checkpoint committed at `state` over `ledger`: the journal starts
    /// again empty.
    fn checkpointed(&mut self, state: ParkedState, ledger: EffectLedger) {
        *self = Self::following(CellJournal::default(), Some((state, ledger)));
    }
}

/// Each call `journal` admitted, with its Admission node.
fn journal_calls(journal: &CellJournal) -> Vec<(lash_protocol_types::ToolCallId, NodeId)> {
    journal
        .admitted
        .values()
        .filter_map(|resolved| match &resolved.admission.disposition {
            super::graph::Disposition::Execution { pin, .. } => {
                Some((pin.call.clone(), resolved.node))
            }
            super::graph::Disposition::Refused => None,
        })
        .collect()
}

/// What of a park a run that follows its journal must match.
#[derive(Clone, Copy, Debug)]
enum Field {
    /// The effect, its operation, its wait or its call identity.
    Call,
    /// The frozen request: the call's arguments.
    Arguments,
    /// How many effects it requested against what the journal admitted.
    Issued,
    /// The run's cumulative exact charge.
    Charge,
    /// How many values it printed.
    Prints,
    /// The document or execution closure it runs.
    Code,
}

impl Field {
    fn name(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Arguments => "arguments",
            Self::Issued => "issued count",
            Self::Charge => "charge",
            Self::Prints => "prints",
            Self::Code => "document or closure",
        }
    }
}

/// The typed refusal of a run that left its journal at park `park`: the
/// field that differed, and how.
fn diverged(park: u64, field: Field, detail: impl std::fmt::Display) -> KernelFailure {
    KernelFailure::Diverged(format!(
        "the run left its journal at park {park}: its {} differs ({detail})",
        field.name()
    ))
}

#[async_trait::async_trait]
impl<S: Machines> lash_core_execution::kernel_runner::KernelOwner for CellKernelOwner<'_, '_, S> {
    type Error = KernelFailure;
    type Output = CellStop;

    fn bounds(&self) -> Bounds {
        self.machines.bounds()
    }

    fn cancel_requests(&mut self, waits: &[lash_kernel_value::sites::WaitCancellation]) {
        for wait in waits {
            self.ledger.cancel_wait(*wait);
        }
    }

    async fn next(&mut self) -> Result<MachineStep, KernelFailure> {
        if self.ledger.has_cancellations() {
            self.broker.store.apply_cancellations().await?;
        }
        if self.machine.is_none() {
            // A machine that gave its worker back starts again from the
            // run's last checkpoint and follows everything this activation
            // knows of its run since.
            self.journal.restart();
            self.broker.effects.restart();
            self.broker.store.reload_records().await;
            let base = self.journal.base.as_ref();
            self.ledger = base.map(|(_, ledger)| ledger.clone()).unwrap_or_default();
            let state = base.map(|(state, _)| state);
            let previous = std::mem::replace(
                &mut self.activation,
                self.broker
                    .activation(state, self.binding, self.machines)
                    .await?,
            );
            self.broker.end_activation(previous).await;
            let pages = self.activation.pages.clone();
            self.machine = Some(match state {
                Some(state) => self.machines.resume(state, self.binding, pages).await?,
                None => self.machines.start(self.binding, pages).await?,
            });
        }
        let running = self
            .machine
            .as_mut()
            .ok_or(KernelFailure::EmptyCheckpoint)?;
        super::pages::shared::while_held(
            &self.activation.pages,
            running.run(self.broker.slice, self.cancel.is_cancelled()),
        )
        .await
    }

    async fn sliced(&mut self) -> Result<Option<CellStop>, KernelFailure> {
        self.broker.commit_response_alone(&mut self.held).await?;
        Ok(None)
    }

    async fn end(&mut self, end: End) -> Result<CellStop, KernelFailure> {
        if let Some(machine) = self.machine.take() {
            machine.release().await?;
        }
        Ok(CellStop::Ended(end))
    }

    async fn park(&mut self, park: MachinePark) -> Result<Option<CellStop>, KernelFailure> {
        self.park_journaled(park).await
    }
}

impl<S: Machines> CellKernelOwner<'_, '_, S> {
    /// How far the run has come: its cumulative exact charge, from the
    /// machine's latest receipt, and the values it printed.
    fn progress(&self) -> super::graph::ParkProgress {
        super::graph::ParkProgress {
            charge: self
                .machine
                .as_ref()
                .and_then(DrivenMachine::meters)
                .and_then(|meters| meters.charged),
            prints: self.broker.effects.prints(),
        }
    }

    /// An in-turn run's park (FIG-7585): its requests are admitted, or,
    /// while the machine follows its journal, matched against what the
    /// journal admitted; then the machine takes its next recorded batch, or
    /// once it has taken them all, what settles.
    async fn park_journaled(
        &mut self,
        park: MachinePark,
    ) -> Result<Option<CellStop>, KernelFailure> {
        let cancels = !park.cancel_requests.is_empty();
        let requests = park.requests;
        let (outstanding, live) = (park.outstanding, park.live);
        self.journal.parks += 1;
        let at = self.journal.parks;
        let journal = &self.journal;
        let recorded = requests
            .iter()
            .filter(|Request::Effect(request)| {
                journal.journal.admitted.contains_key(&request.identity)
            })
            .count();
        let replayed = !requests.is_empty() && recorded == requests.len();
        if recorded != 0 && !replayed {
            return Err(diverged(
                at,
                Field::Issued,
                format_args!(
                    "{recorded} of its {} effects are ones the journal admitted",
                    requests.len()
                ),
            ));
        }
        if replayed {
            self.replay_requests(at, requests, outstanding, live)
                .await?;
        } else if !requests.is_empty() || cancels {
            if !requests.is_empty() && journal.matched < journal.journal.admitted.len() {
                return Err(diverged(
                    at,
                    Field::Issued,
                    format_args!(
                        "a new effect after {} of the {} its journal admitted",
                        journal.matched,
                        journal.journal.admitted.len()
                    ),
                ));
            }
            let stop = if !requests.is_empty() && self.journal.due() {
                self.checkpoint(requests, outstanding, live).await?
            } else {
                self.commit_journaled(requests, outstanding, live).await?
            };
            if let Some(stop) = stop {
                return Ok(Some(stop));
            }
        }
        // The machine settles the waits it cancels itself, as it decided
        // to: a replay decides the same.
        if cancels {
            self.broker.store.apply_cancellations().await?;
            return Ok(None);
        }
        if self.ledger.pending().next().is_none() {
            return Err(KernelFailure::Stalled);
        }
        self.broker.commit_response_alone(&mut self.held).await?;
        let journal = &mut self.journal;
        if !journal.delivered_all() {
            // The machine takes the batch its run took here, whole and in
            // its order: every outcome in it committed before it was
            // delivered.
            let batch = journal.journal.batches[journal.batch].clone();
            journal.batch += 1;
            let outcome = |effect: &AdmittedEffect, output: &SettledOutput| {
                self.broker.effects.outcome(effect, output)
            };
            let settled = self.broker.store.settled_as(&self.ledger, &outcome).await?;
            let running = self
                .machine
                .as_mut()
                .ok_or(KernelFailure::EmptyCheckpoint)?;
            for effect in &batch {
                let outcome = settled
                    .iter()
                    .find(|settled| &settled.identity == effect)
                    .ok_or_else(|| {
                        diverged(
                            at,
                            Field::Call,
                            format_args!(
                                "the journal delivers effect {effect} here, which the run does \
                                 not stand on or whose outcome is not committed"
                            ),
                        )
                    })?;
                running
                    .deliver(outcome.wait, outcome.outcome.clone())
                    .await?;
                self.ledger.consume(effect);
            }
            return Ok(None);
        }
        let quiet = tokio::sync::Notify::new();
        let driven = {
            let outcome = |effect: &AdmittedEffect, output: &SettledOutput| {
                self.broker.effects.outcome(effect, output)
            };
            let drive = self.broker.store.drive_effects_as(
                &self.ledger,
                self.cancel,
                &outcome,
                Some(&quiet),
            );
            tokio::pin!(drive);
            loop {
                tokio::select! {
                    driven = &mut drive => break driven?,
                    // A machine that only waits gives its worker back: it
                    // starts again when its outcomes come (FIG-4275).
                    () = quiet.notified(), if self.machine.is_some() => {
                        if let Some(waiting) = self.machine.take() {
                            waiting.release().await?;
                        }
                    }
                    () = renew_when_due(&self.activation.pages), if self.machine.is_some() => {}
                }
            }
        };
        let settled = match driven {
            Driven::Answered(settled) => settled,
            Driven::Suspended => {
                if let Some(parked) = self.machine.take() {
                    parked.release().await?;
                }
                return Ok(Some(CellStop::Suspended));
            }
        };
        if self.broker.effects.stop_requested() {
            if let Some(stopped) = self.machine.take() {
                stopped.release().await?;
            }
            return lash_core_execution::kernel_runner::KernelOwner::end(self, End::Cancelled)
                .await
                .map(Some);
        }
        let journal = &mut self.journal;
        let batch = u64::try_from(journal.journal.batches.len()).unwrap_or(u64::MAX);
        let mut effects = Vec::with_capacity(settled.len());
        for (position, settled) in settled.iter().enumerate() {
            journal.pending.push((
                settled.identity.clone(),
                lash_durable::domain::Delivery {
                    batch,
                    position: u32::try_from(position).unwrap_or(u32::MAX),
                },
            ));
            effects.push(settled.identity.clone());
        }
        journal.journal.batches.push(effects);
        journal.batch += 1;
        // A machine that gave its worker back starts again and follows
        // this batch with the rest.
        let Some(running) = self.machine.as_mut() else {
            return Ok(None);
        };
        for outcome in settled {
            running.deliver(outcome.wait, outcome.outcome).await?;
            self.ledger.consume(&outcome.identity);
        }
        Ok(None)
    }

    /// Stand the machine on `requests`, each of which its journal admitted,
    /// as that admission did: the host takes each up again as it did, and
    /// each must be the effect, request, call and park the journal holds.
    async fn replay_requests(
        &mut self,
        at: u64,
        requests: Vec<Request>,
        outstanding: u64,
        live: u64,
    ) -> Result<(), KernelFailure> {
        // The run's lifetime counts the requests it replays as it counted
        // them when it admitted them, so a bound recurs at the same park.
        let growth = self
            .broker
            .ceilings
            .grow(self.ledger.growth(), requests.len() as u64, 0)
            .map_err(|_| {
                diverged(
                    at,
                    Field::Issued,
                    "the journal admitted effects past the run's lifetime bound",
                )
            })?;
        self.ledger.grown(growth);
        let park = self.ledger.next_park();
        let mut executions = 0_u64;
        let count = requests.len();
        // The run stands where its first run stood at this park: the same
        // cumulative charge (the kernel meters it deterministically) and as
        // many prints, so compute between parks that diverged is refused
        // even when the next call matches (FIG-7585).
        let progress = self.progress();
        for Request::Effect(request) in requests {
            let journal = &self.journal;
            let recorded = journal
                .journal
                .admitted
                .get(&request.identity)
                .ok_or(KernelFailure::EmptyCheckpoint)?
                .clone();
            let recorded_progress = recorded.admission.progress;
            if let (Some(then), Some(now)) = (recorded_progress.charge, progress.charge)
                && then != now
            {
                return Err(diverged(
                    at,
                    Field::Charge,
                    format_args!("{now} charge units where its journal recorded {then}"),
                ));
            }
            if recorded_progress.prints != progress.prints {
                return Err(diverged(
                    at,
                    Field::Prints,
                    format_args!(
                        "{} prints where its journal recorded {}",
                        progress.prints, recorded_progress.prints
                    ),
                ));
            }
            if recorded.admission.wait != request.wait
                || recorded.admission.effect_name != Some(request.use_)
            {
                return Err(diverged(
                    at,
                    Field::Call,
                    format_args!(
                        "effect {} waits as another wait or performs another operation than \
                         its journal admitted",
                        request.identity
                    ),
                ));
            }
            let call = self.broker.identities.child_call_id(park, executions);
            let standing = match (&recorded.admission.disposition, &recorded.request) {
                // A call its journal admitted stands admitted under the
                // contract its admission pinned: it is judged by that
                // contract alone when it runs, never admitted again against
                // this build's surface (OP-LINK-ONE, FIG-7611).
                (
                    super::graph::Disposition::Execution { operation, pin },
                    RecordedRequest::Tool { args, .. },
                ) => {
                    if pin.call != call || operation.run != park {
                        return Err(diverged(
                            at,
                            Field::Call,
                            format_args!(
                                "effect {} is not the call its journal admitted",
                                request.identity
                            ),
                        ));
                    }
                    if *args != request.args {
                        return Err(diverged(
                            at,
                            Field::Arguments,
                            format_args!(
                                "effect {} is called with other arguments than its journal \
                                 admitted",
                                request.identity
                            ),
                        ));
                    }
                    self.broker
                        .effects
                        .readmit(&request, &recorded.request)
                        .await?;
                    executions += 1;
                    Standing::Admitted(AdmittedEffect {
                        operation: *operation,
                        call,
                    })
                }
                (_, recorded_request) => match self
                    .broker
                    .effects
                    .admit(&request, call.clone(), outstanding, live)
                    .await?
                {
                    Ok(_) => {
                        return Err(diverged(
                            at,
                            Field::Call,
                            format_args!(
                                "effect {} is admitted where its journal refused it",
                                request.identity
                            ),
                        ));
                    }
                    Err(error) => match recorded_request {
                        RecordedRequest::Refused {
                            error: recorded,
                            args,
                        } if *recorded == error && *args == request.args => {
                            Standing::Refused(error)
                        }
                        _ => {
                            return Err(diverged(
                                at,
                                Field::Call,
                                format_args!(
                                    "effect {} is refused otherwise than its journal recorded",
                                    request.identity
                                ),
                            ));
                        }
                    },
                },
            };
            if !self.ledger.stand(
                request.identity.clone(),
                PendingEffect {
                    wait: request.wait,
                    effect: Some(request.use_),
                    standing,
                    cancellation_requested: false,
                },
            ) {
                return Err(diverged(
                    at,
                    Field::Call,
                    format_args!("it requests effect {} twice", request.identity),
                ));
            }
        }
        if executions > 0 {
            self.ledger.take_park();
        }
        {
            let journal = &mut self.journal;
            journal.matched += count;
        }
        Ok(())
    }

    /// Commit a park as the run's checkpoint (FIG-7585): its sealed
    /// state, the admission of `requests` and the cancel decisions the
    /// ledger holds, and every admission its journal held covered. The
    /// journal starts again empty, and a machine that gives its worker back
    /// from here resumes from this state.
    async fn checkpoint(
        &mut self,
        requests: Vec<Request>,
        outstanding: u64,
        live: u64,
    ) -> Result<Option<CellStop>, KernelFailure> {
        // The run's first batch committed the response its turn held.
        self.broker.commit_response_alone(&mut self.held).await?;
        let running = self
            .machine
            .as_mut()
            .ok_or(KernelFailure::EmptyCheckpoint)?;
        let state = self
            .broker
            .sealed(
                running,
                &self.activation,
                self.binding,
                &self.document,
                self.machines.closure()?.reference(),
            )
            .await?;
        // A checkpoint appends the pages the activation sealed since the
        // last one to the run's lifetime growth.
        let sealed = self.activation.pages.lock().await.take_sealed_bytes();
        match self
            .broker
            .ceilings
            .grow(self.ledger.growth(), requests.len() as u64, sealed)
        {
            Ok(growth) => self.ledger.grown(growth),
            Err(exceeded) => {
                if let Some(refused) = self.machine.take() {
                    refused.release().await?;
                }
                let end = End::Error(RunError::Bound(exceeded));
                return lash_core_execution::kernel_runner::KernelOwner::end(self, end)
                    .await
                    .map(Some);
            }
        }
        let admit = self
            .broker
            .admissions(&self.ledger, requests, outstanding, live, self.progress())
            .await?;
        let save = ParkSave {
            document: self.document,
            state: state.clone(),
            pages: &self.activation.pages,
            closure: self.machines.closure()?,
            carried: self.machines.carried(),
            ledger: std::mem::replace(&mut self.ledger, EffectLedger::new()),
            admit,
            with: vec![DomainWrite::EffectAdmission(EffectAdmissionWrite::Cover {
                exec: self.broker.store.exec().clone(),
            })],
        };
        let committed = self
            .broker
            .store
            .commit_park(save, self.broker.effects)
            .await?;
        self.saved = true;
        self.ledger = committed.ledger;
        let resting = self.broker.resting(committed.root, state, &self.ledger)?;
        self.journal.checkpointed(resting, self.ledger.clone());
        self.broker.effects.mark();
        Ok(None)
    }

    /// Commit an in-turn park: the admission of `requests`, the cancel
    /// decisions the ledger holds and the deliveries and reads since the
    /// last commit, with the turn's held response when this is the run's
    /// first. `Some` when the turn's cancel stopped the run at that commit.
    async fn commit_journaled(
        &mut self,
        requests: Vec<Request>,
        outstanding: u64,
        live: u64,
    ) -> Result<Option<CellStop>, KernelFailure> {
        let growth = self
            .broker
            .ceilings
            .grow(self.ledger.growth(), requests.len() as u64, 0);
        match growth {
            Ok(growth) => self.ledger.grown(growth),
            Err(exceeded) => {
                if let Some(refused) = self.machine.take() {
                    refused.release().await?;
                }
                let end = End::Error(RunError::Bound(exceeded));
                return lash_core_execution::kernel_runner::KernelOwner::end(self, end)
                    .await
                    .map(Some);
            }
        }
        let admit = self
            .broker
            .admissions(&self.ledger, requests, outstanding, live, self.progress())
            .await?;
        let identities: Vec<EffectId> = admit.iter().map(|admit| admit.identity.clone()).collect();
        let delivered = self.journal.pending.clone();
        let save = super::store::JournaledPark {
            document: self.document,
            closure: self.machines.closure()?,
            ledger: std::mem::replace(&mut self.ledger, EffectLedger::new()),
            admit,
            delivered,
            with: Vec::new(),
        };
        let held = self.held.take();
        match self
            .broker
            .store
            .commit_journaled(save, self.broker.effects, held)
            .await?
        {
            super::store::Journaled::Committed(ledger) => {
                self.ledger = ledger;
                self.saved |= !identities.is_empty();
                {
                    let journal = &mut self.journal;
                    journal.pending.clear();
                    for identity in identities {
                        let resolved = self.broker.store.resolved(&identity).ok_or_else(|| {
                            KernelFailure::Checkpoint(crate::snapshot::refused(
                                "a committed admission is not in the activation's book",
                            ))
                        })?;
                        journal.journal.admitted.insert(identity, resolved);
                        journal.matched += 1;
                    }
                }
                Ok(None)
            }
            // The turn's cancel stopped the cell at its first commit:
            // nothing it admitted starts, and the turn, which drops this
            // run, stops where it would have before the cell started.
            super::store::Journaled::Stopped => {
                if let Some(stopped) = self.machine.take() {
                    stopped.release().await?;
                }
                if let Ok(store) = self.broker.store.context().pages() {
                    end_activation(store.as_ref(), &self.activation.pages).await;
                }
                if let Some(response) = held {
                    response.stop();
                }
                std::future::pending().await
            }
        }
    }
}

/// The root `running` seals at this safe point, its header read from the
/// activation's spool through `pages` and checked: this kernel, this
/// document, bound to the OwnerBinding node `binding`, pinning `closure`,
/// the document's execution closure.
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
    closure: NodeId,
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
    if header.closure.0 != closure.0 {
        return Err(CheckpointRefusal::WrongClosure {
            expected: closure.to_string(),
            found: NodeId(header.closure.0).to_string(),
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
            memory_meter: lash_kernel_vm::MemoryMeter::On,
            charge: lash_kernel_vm::Charge::Exact,
            charge_limit: u64::MAX,
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
            effects: u64::MAX,
            checkpoint_bytes: u64::MAX,
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
                site: None,
            })
        );
    }
}
