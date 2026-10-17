//! A kernel run's machine in a pooled worker process, as the broker drives
//! it.
//!
//! The worker hosts one machine for one execution (kernel spec §9 rule 5).
//! The machine's host is synchronous, so each host read crosses to the
//! parent in the middle of a `run` exchange and is answered here by a
//! [`RunHost`]: the clock, the random source, and the provider registered
//! for a projection handle. What the run prints arrives with the exchange
//! that printed it.

use std::sync::Arc;

use lash_core_store::checkpoint_graph::NodeId;
use lash_kernel_doc::heap::PageGrant;
use lash_kernel_doc::{
    Datum, DocumentId, EffectName, ErrorDatum, Handle, IdentifiedDocument, Site, Timestamp,
};
use lash_kernel_state::paged::SessionRoot;
use lash_kernel_vm::{Bounds, EffectModes, End, Outcome, Target, WaitId};
use lash_vm_broker::ParentFault;
use lash_vm_broker::kernel::pages::{PageExchange, SharedPages};
use lash_vm_broker::kernel::state::{GraphStateFailure, ReleaseGraph};
use lash_vm_broker::kernel::{
    DrivenMachine, KernelFailure, MachinePark, MachineStep, Machines, ParkedState,
};
use lash_vm_protocol::page::{PageReply, SealBatch};
use lash_vm_protocol::{
    CheckpointStart, EncodedPayload, FrameEpoch, HostReadKind, InfrastructureOutcome, OwnerEpoch,
    PayloadKind, RunBounds, StartFrom, VmOwner,
};

use crate::service::{CallHeld, Service};
use crate::wire::{
    self, CarryWire, EffectRead, EndWire, OutcomeWire, PageLink, ParkWire, ProjectionRead,
    StartWire,
};
use crate::{Checkout, ExecutionClass, PoolError, RunStep, WorkerPoolRuntimeOps as _};

pub use lash_vm_broker::kernel::run::kernel_reads;

/// What a new run starts from: a [`lash_kernel_vm::CellStart`] less the
/// owner binding, which the broker supplies per execution.
#[derive(Clone, Debug)]
pub struct RunStart {
    pub target: Target,
    /// The entry's arguments. Empty for `main`.
    pub args: Vec<Datum>,
    /// The carried session a cell opens in place, its pages readable under
    /// the activation's lease.
    pub session: Option<SessionRoot>,
    /// The effects a fresh run performs as reads; a resumed run's root
    /// holds its own.
    pub modes: EffectModes,
    /// The host's read-only bindings a cell's `main` starts with.
    pub host: Vec<(lash_kernel_doc::Name, Datum)>,
    /// How a session cell's end saves its functions, fresh or resumed;
    /// none for a run that saves none.
    pub save: Option<wire::SaveWire>,
}

/// What a run reads from its host, answered in the parent.
#[async_trait::async_trait]
pub trait RunHost: Send + Sync {
    /// The run segment's observation option, independent of live delivery.
    fn observe(&self) -> lash_kernel_vm::Observe {
        lash_kernel_vm::Observe::Off
    }

    /// Exact provisional counts since the worker's previous frame.
    fn ran(&self, _delta: &lash_kernel_vm::RanDelta) {}

    /// Whether the run records its statement path, for a replay: the run
    /// frames carry it and [`RemoteMachine::take_path`] answers it.
    fn traces_path(&self) -> bool {
        false
    }

    /// Whether the run reports the document sites it enters. The worker
    /// carries them on the run frames it already sends, so observing them
    /// adds no exchange.
    fn observes_sites(&self) -> bool {
        false
    }

    /// A provisional site entry: each task's last since the run's previous
    /// frame, taken from the next one. It neither yields a task nor admits
    /// work.
    fn enter_site(&self, _at: &lash_kernel_doc::EffectIdentity) {}

    /// The time, read once for each `clock` call the run makes.
    async fn clock(&self) -> Result<Timestamp, ParentFault>;

    /// Sixty-four random bits.
    async fn random(&self) -> Result<u64, ParentFault>;

    /// A read through a projection handle. `Err` is raised in the guest,
    /// which may catch it.
    async fn read(
        &self,
        handle: &Handle,
        request: &Datum,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault>;

    /// A `perform` the run binds as a read (`K-EFF-012`), at `site`: the
    /// effect's result, or the error raised at the `perform`, which the
    /// guest may catch. `Err` is the parent's own failure: the run stops
    /// there, unanswered, and its last save stands.
    async fn perform(
        &self,
        effect: &EffectName,
        args: &[Datum],
        site: &Site,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault>;

    /// One `print` call's value, in the order the run printed.
    fn print(&self, value: Datum) -> Result<(), ParentFault>;
}

/// One run's machines: each a checkout of `service`'s pool, started on
/// `document` or resumed from the run's parked state.
pub struct RemoteMachines {
    pub service: Service,
    pub owner: VmOwner,
    pub owner_epoch: OwnerEpoch,
    pub frame_epoch: FrameEpoch,
    pub host: Arc<dyn RunHost>,
    document: DocumentId,
    /// The dialect the run's document is written in, as its session or
    /// process recorded it: the worker refuses one its embedding lacks, and
    /// a new run pins that dialect's helper release.
    dialect: String,
    /// The document's token body, as every checkout's `Start` carries it.
    body: Arc<[u8]>,
    start: RunStart,
    bounds: Bounds,
    releases: std::sync::OnceLock<Result<ReleaseGraph, GraphStateFailure>>,
}

impl RemoteMachines {
    /// The run of `document`, written in `dialect`, from `start`, held to
    /// `bounds`.
    /// `dialect` is recorded configuration: what the run's session or
    /// process recorded at creation, never a live read. The document
    /// crosses to each checkout as the token body it already holds:
    /// nothing is encoded here.
    ///
    /// # Errors
    ///
    /// [`KernelFailure`] when `start`'s host metadata is refused.
    #[expect(clippy::too_many_arguments, reason = "one run's whole identity")]
    pub fn new(
        service: Service,
        owner: VmOwner,
        owner_epoch: OwnerEpoch,
        frame_epoch: FrameEpoch,
        host: Arc<dyn RunHost>,
        document: &IdentifiedDocument,
        dialect: &str,
        start: RunStart,
        bounds: Bounds,
    ) -> Result<Self, KernelFailure> {
        lash_kernel_vm::CellStart::admit_host_metadata(
            start.host.iter().map(|(name, _)| name.as_str()),
        )?;
        Ok(Self {
            service,
            owner,
            owner_epoch,
            frame_epoch,
            host,
            document: document.id(),
            dialect: dialect.to_owned(),
            body: Arc::clone(document.shared_body()),
            start,
            bounds,
            releases: std::sync::OnceLock::new(),
        })
    }

    /// A checkout holding the run: the one that lowered its document when
    /// the execution kept it, and otherwise one from the pool. Every root it
    /// seals is bound to the OwnerBinding node `binding`; its pages cross
    /// through `pages`.
    async fn checkout(
        &self,
        from: StartFrom,
        binding: NodeId,
        pages: SharedPages,
        roots: &[NodeId],
    ) -> Result<RemoteMachine, KernelFailure> {
        // Prefetch changes latency only: pages it cannot carry are left to
        // the worker's own reads, which meet any fault where it is typed.
        let warm = pages
            .lock()
            .await
            .warm(roots, lash_vm_broker::kernel::pages::WARM_BYTES)
            .await
            .unwrap_or_default();
        let start = lash_vm_protocol::Start {
            owner: self.owner.clone(),
            dialect: self.dialect.clone(),
            document: EncodedPayload(self.body.to_vec()),
            from,
            bounds: RunBounds {
                charge: self.bounds.charge,
                memory: self.bounds.memory,
                call_depth: self.bounds.call_depth,
                live_tasks: self.bounds.live_tasks,
                requests_per_park: self.bounds.requests_per_park,
                join_members: self.bounds.join_members,
            },
            binding: binding.0,
            save: self
                .start
                .save
                .as_ref()
                .map(|save| wire::encode(PayloadKind::Start, save))
                .transpose()
                .map_err(worker)?,
            warm,
            observe_sites: self.host.observes_sites(),
            observe_counts: self.host.observe() == lash_kernel_vm::Observe::Counts,
            observe_path: self.host.traces_path(),
        };
        let reservation = start_reservation(
            &start,
            &lash_vm_protocol::FrameCodec::new(self.service.config().protocol.decode),
        )
        .map_err(worker)?;
        let service = self.service.clone();
        let (owner_epoch, frame_epoch) = (self.owner_epoch, self.frame_epoch);
        let document = self.document.to_string();
        let class = match self.start.target {
            lash_kernel_vm::Target::Main => ExecutionClass::Cell,
            lash_kernel_vm::Target::Entry(_) => ExecutionClass::Process,
        };
        let held = CallHeld::of(Some(&self.service));
        let checkout = blocking(move || {
            let mut checkout = match service.take_retained(owner_epoch, frame_epoch) {
                Some(mut retained) => match retained.readmit(reservation) {
                    Ok(()) => retained,
                    Err(refused) => {
                        // The worker goes back to the pool, as a checkout
                        // refused under the budget leaves it; a reset that
                        // fails has discarded it instead.
                        let _ = retained.release();
                        return Err(refused);
                    }
                },
                None => service.pool()?.checkout(
                    reservation,
                    owner_epoch,
                    frame_epoch,
                    service.budget.clone().unwrap_or_default(),
                )?,
            };
            #[cfg(feature = "testing")]
            service.record_worker(
                match class {
                    ExecutionClass::Cell => crate::service::WorkerPath::Cell,
                    ExecutionClass::Process => crate::service::WorkerPath::Process,
                },
                &checkout,
            )?;
            checkout.start(start, &document, class)?;
            Ok(checkout)
        })
        .await?;
        drop(held);
        Ok(RemoteMachine {
            checkout: Some(checkout),
            service: self.service.clone(),
            host: Arc::clone(&self.host),
            pages,
            candidate: None,
            ran: Default::default(),
            path: Vec::new(),
        })
    }
}

#[async_trait::async_trait]
impl Machines for RemoteMachines {
    type Machine = RemoteMachine;

    fn document(&self) -> DocumentId {
        self.document
    }

    fn bounds(&self) -> Bounds {
        self.bounds
    }

    fn releases(&self) -> Result<&ReleaseGraph, KernelFailure> {
        self.releases
            .get_or_init(lash_vm_broker::kernel::state::release_manifests)
            .as_ref()
            .map_err(|failure| KernelFailure::State(failure.clone()))
    }

    async fn start(
        &self,
        binding: NodeId,
        pages: SharedPages,
    ) -> Result<RemoteMachine, KernelFailure> {
        let session = {
            let service = pages.lock().await;
            self.start.session.as_ref().map(|session| {
                CarryWire::of(session, |page| {
                    PageLink::Granted(service.issue(NodeId(page.id.0)))
                })
            })
        };
        let start = wire::encode(
            PayloadKind::Start,
            &StartWire::of(
                self.start.target.clone(),
                self.start.args.clone(),
                session,
                &self.start.modes,
                self.start.host.clone(),
            ),
        )
        .map_err(worker)?;
        // A session cell reads its carried roots first.
        let roots: Vec<NodeId> = self
            .start
            .session
            .as_ref()
            .map(|session| {
                std::iter::once(session.heap)
                    .chain(session.bindings.page())
                    .chain(session.functions.page())
                    .map(|page| NodeId(page.id.0))
                    .collect()
            })
            .unwrap_or_default();
        self.checkout(StartFrom::Fresh(start), binding, pages, &roots)
            .await
    }

    /// Opens the run's committed checkpoint root: the worker reads its
    /// header, then only the pages its next steps touch, under the
    /// activation's lease (FIG-6190).
    async fn resume(
        &self,
        parked: &ParkedState,
        binding: NodeId,
        pages: SharedPages,
    ) -> Result<RemoteMachine, KernelFailure> {
        let root = pages.lock().await.issue(parked.root);
        let from = StartFrom::Checkpoint(CheckpointStart {
            root,
            admissions: parked
                .admissions
                .iter()
                .map(lash_vm_broker::kernel::state::admission)
                .collect(),
        });
        self.checkout(from, binding, pages, &[parked.root]).await
    }
}

impl RemoteMachines {
    /// Whose runs these machines are.
    #[must_use]
    pub fn owner(&self) -> &VmOwner {
        &self.owner
    }
}

/// One run's machine in a checked-out worker. Dropping it mid-run discards
/// the worker.
pub struct RemoteMachine {
    checkout: Option<Checkout>,
    service: Service,
    host: Arc<dyn RunHost>,
    /// The activation's page service, which answers the heap's page
    /// exchanges in this process: never another worker (FIG-6190).
    pages: SharedPages,
    /// The root the last park sealed, until a delivery or a slice moves
    /// the run on.
    candidate: Option<NodeId>,
    ran: std::collections::BTreeMap<u32, u64>,
    path: wire::OrdinalPath,
}

/// Where a worker's exchange stopped once its host reads and page
/// exchanges were answered.
enum Stop {
    Parked {
        park: EncodedPayload,
        candidate: Option<NodeId>,
    },
    Slice,
    Ended {
        end: EncodedPayload,
        /// The pages that crossed with the end, admitted.
        carried: Vec<NodeId>,
    },
    Sealed(NodeId),
}

impl RemoteMachine {
    /// Takes and resets the total received in this run segment's frames.
    pub fn take_ran(&mut self) -> lash_kernel_vm::RanDelta {
        std::mem::take(&mut self.ran).into_iter().collect()
    }

    /// Takes and resets the statement path received in this run segment's
    /// frames, in order, when the host traces it.
    pub fn take_path(&mut self) -> wire::OrdinalPath {
        std::mem::take(&mut self.path)
    }

    /// The candidate root a worker named, when this activation staged it.
    async fn staged(pages: &SharedPages, candidate: [u8; 32]) -> Result<NodeId, KernelFailure> {
        pages
            .lock()
            .await
            .staged(NodeId(candidate))
            .map_err(refused)
    }

    /// Admits the seal batch that crossed with a step, as a seal exchange
    /// of the same pages is admitted, and holds its grants for the run's
    /// next slice. The admitted pages, in batch order.
    async fn admit(&mut self, seal: Option<SealBatch>) -> Result<Vec<NodeId>, KernelFailure> {
        let Some(batch) = seal else {
            return Ok(Vec::new());
        };
        let carried: Vec<NodeId> = batch.pages.iter().map(|page| NodeId(page.id)).collect();
        let admitted = self.pages.lock().await.admit(batch).await;
        match admitted {
            Ok(grants) => {
                if let Some(checkout) = self.checkout.as_mut() {
                    checkout.grant(grants);
                }
                Ok(carried)
            }
            Err(refusal) => {
                // The activation ends here, as infrastructure, as a refused
                // seal exchange ends it.
                self.checkout = None;
                Err(refused(refusal))
            }
        }
    }

    /// The run's end: a finished cell's carry names only pages this
    /// activation may read, or pages that crossed with the end.
    async fn end(
        pages: &SharedPages,
        end: EndWire,
        carried: &[NodeId],
    ) -> Result<End, KernelFailure> {
        let pages = pages.lock().await;
        end.into_end(|link| {
            match link {
                PageLink::Granted(capability) => pages.verify(capability).map(|_| PageGrant::NONE),
                PageLink::Carried(page) if carried.contains(&NodeId(*page)) => Ok(PageGrant::NONE),
                PageLink::Carried(_) => Err(lash_vm_protocol::page::PageRefusal::Unauthorized),
            }
            .map_err(|refusal| PoolError::from(InfrastructureOutcome::PageRefused { refusal }))
        })
        .map_err(worker)
    }

    /// Answers the worker's host reads and page exchanges from `step` on,
    /// until it stops.
    async fn drive(&mut self, mut step: RunStep) -> Result<Stop, KernelFailure> {
        loop {
            self.printed()?;
            self.entered()?;
            match step {
                RunStep::HostRead { id, kind, request } => {
                    let host = Arc::clone(&self.host);
                    let scope = self.checkout.as_ref().and_then(Checkout::phase_scope);
                    let started = scope.as_ref().map(|scope| scope.recorder.now());
                    let answer = answer(host.as_ref(), kind, &request).await;
                    if let (Some(scope), Some(start)) = (scope, started) {
                        scope.record("host_body", start);
                    }
                    let answer = answer?;
                    step = self
                        .with(move |checkout| checkout.host_answer(id, answer))
                        .await?;
                }
                RunStep::Pages { id, request } => {
                    let reply = self.pages.serve(request).await;
                    if let PageReply::Refused(refusal) = reply {
                        // The activation ends here, as infrastructure; the
                        // worker that waits on the pages is discarded.
                        self.checkout = None;
                        return Err(KernelFailure::Worker {
                            outcome: InfrastructureOutcome::PageRefused { refusal },
                        });
                    }
                    step = self
                        .with(move |checkout| checkout.page_reply(id, reply))
                        .await?;
                }
                RunStep::Parked {
                    park,
                    seal,
                    candidate,
                    ..
                } => {
                    self.admit(seal).await?;
                    let candidate = match candidate {
                        Some(root) => Some(Self::staged(&self.pages, root).await?),
                        None => None,
                    };
                    return Ok(Stop::Parked { park, candidate });
                }
                RunStep::Slice { .. } => return Ok(Stop::Slice),
                RunStep::Ended { end, seal, .. } => {
                    let carried = self.admit(seal).await?;
                    return Ok(Stop::Ended { end, carried });
                }
                RunStep::Sealed { seal, candidate } => {
                    self.admit(seal).await?;
                    return Ok(Stop::Sealed(Self::staged(&self.pages, candidate).await?));
                }
            }
        }
    }

    /// One blocking exchange on the checkout, off the async runtime.
    async fn with<T: Send + 'static>(
        &mut self,
        exchange: impl FnOnce(&mut Checkout) -> Result<T, PoolError> + Send + 'static,
    ) -> Result<T, KernelFailure> {
        let mut checkout = self
            .checkout
            .take()
            .ok_or_else(|| worker(PoolError::eof()))?;
        let held = CallHeld::of(Some(&self.service));
        let (checkout, result) = blocking(move || {
            let result = exchange(&mut checkout);
            Ok((checkout, result))
        })
        .await?;
        drop(held);
        // A failed exchange has already discarded the worker; the checkout
        // is kept only so its drop accounts for it.
        self.checkout = Some(checkout);
        let result = result.map_err(worker);
        if result.is_err() {
            self.checkout = None;
        }
        result
    }

    fn printed(&mut self) -> Result<(), KernelFailure> {
        let Some(checkout) = self.checkout.as_mut() else {
            return Ok(());
        };
        for payload in checkout.take_printed() {
            // One frame's values, each its own JSON text.
            let values: Vec<serde_bytes::ByteBuf> = rmp_serde::from_slice(&payload.0)
                .map_err(|error| worker(PoolError::payload(PayloadKind::Printed, error)))?;
            for value in values {
                self.host
                    .print(wire::from_json(PayloadKind::Printed, &value).map_err(worker)?)?;
            }
        }
        Ok(())
    }

    fn entered(&mut self) -> Result<(), KernelFailure> {
        let Some(checkout) = self.checkout.as_mut() else {
            return Ok(());
        };
        for payload in checkout.take_sites() {
            let sites: wire::Sites = wire::decode(PayloadKind::Sites, &payload).map_err(worker)?;
            for at in &sites.entered {
                self.host.enter_site(at);
            }
            for &(ordinal, count) in &sites.ran {
                *self.ran.entry(ordinal).or_default() += count;
            }
            if !sites.ran.is_empty() {
                self.host.ran(&sites.ran);
            }
            for (task, starts) in sites.path {
                match self.path.last_mut() {
                    Some((last, held)) if *last == task => held.extend(starts),
                    _ => self.path.push((task, starts)),
                }
            }
        }
        Ok(())
    }
}

async fn answer(
    host: &dyn RunHost,
    kind: HostReadKind,
    request: &EncodedPayload,
) -> Result<EncodedPayload, KernelFailure> {
    match kind {
        HostReadKind::Clock => wire::encode(PayloadKind::HostAnswer, &host.clock().await?),
        HostReadKind::Random => wire::encode(PayloadKind::HostAnswer, &host.random().await?),
        HostReadKind::Projection => {
            let read: ProjectionRead =
                wire::decode(PayloadKind::HostRead, request).map_err(worker)?;
            wire::encode(
                PayloadKind::HostAnswer,
                &host.read(&read.handle, &read.request).await?,
            )
        }
        HostReadKind::Effect => {
            let read: EffectRead = wire::decode(PayloadKind::HostRead, request).map_err(worker)?;
            wire::encode(
                PayloadKind::HostAnswer,
                &host.perform(&read.effect, &read.args, &read.site).await?,
            )
        }
    }
    .map_err(worker)
}

#[async_trait::async_trait]
impl DrivenMachine for RemoteMachine {
    async fn run(&mut self, slice: u64, cancel: bool) -> Result<MachineStep, KernelFailure> {
        self.candidate = None;
        let step = self
            .with(move |checkout| checkout.run(slice, cancel))
            .await?;
        match self.drive(step).await? {
            Stop::Parked { park, candidate } => {
                if candidate.is_some() {
                    self.candidate = candidate;
                }
                let park: ParkWire = wire::decode(PayloadKind::Park, &park).map_err(worker)?;
                Ok(MachineStep::Parked(
                    MachinePark::try_from(park).map_err(worker)?,
                ))
            }
            Stop::Slice => Ok(MachineStep::Slice),
            Stop::Ended { end, carried } => {
                let end: EndWire = wire::decode(PayloadKind::End, &end).map_err(worker)?;
                Ok(MachineStep::Ended(
                    Self::end(&self.pages, end, &carried).await?,
                ))
            }
            Stop::Sealed(_) => Err(unexpected(lash_vm_protocol::WorkerFrameKind::Sealed)),
        }
    }

    async fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<(), KernelFailure> {
        self.candidate = None;
        let outcome =
            wire::encode(PayloadKind::Outcome, &OutcomeWire::from(outcome)).map_err(worker)?;
        self.with(move |checkout| checkout.deliver(wait.0, outcome))
            .await
    }

    /// The root the park sealed when its candidate crossed with it, and
    /// otherwise the worker's answer to a seal.
    async fn seal(&mut self) -> Result<NodeId, KernelFailure> {
        if let Some(root) = self.candidate {
            return Ok(root);
        }
        let step = self.with(Checkout::seal).await?;
        match self.drive(step).await? {
            Stop::Sealed(root) => {
                self.candidate = Some(root);
                Ok(root)
            }
            Stop::Parked { .. } => Err(unexpected(lash_vm_protocol::WorkerFrameKind::Parked)),
            Stop::Slice => Err(unexpected(lash_vm_protocol::WorkerFrameKind::Slice)),
            Stop::Ended { .. } => Err(unexpected(lash_vm_protocol::WorkerFrameKind::Ended)),
        }
    }

    async fn release(mut self) -> Result<(), KernelFailure> {
        let Some(checkout) = self.checkout.take() else {
            return Ok(());
        };
        let held = CallHeld::of(Some(&self.service));
        let released = blocking(move || checkout.release()).await;
        drop(held);
        released
    }
}

fn unexpected(found: lash_vm_protocol::WorkerFrameKind) -> KernelFailure {
    worker(PoolError::breach(
        lash_vm_protocol::ProtocolBreach::Unexpected {
            exchange: lash_vm_protocol::Exchange::Seal,
            found,
        },
    ))
}

fn refused(refusal: lash_vm_protocol::page::PageRefusal) -> KernelFailure {
    worker(PoolError::from(InfrastructureOutcome::PageRefused {
        refusal,
    }))
}

fn worker(error: PoolError) -> KernelFailure {
    KernelFailure::Worker {
        outcome: error.into_outcome(),
    }
}

/// Worker IPC is synchronous; each exchange runs on Tokio's blocking pool.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, PoolError> + Send + 'static,
) -> Result<T, KernelFailure> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| {
            worker(PoolError::breach(
                lash_vm_protocol::ProtocolBreach::Panicked {
                    detail: lash_vm_protocol::Detail::new(error),
                },
            ))
        })?
        .map_err(worker)
}

/// Count the complete Start as it crosses alone. A checkout has not chosen its
/// lease or sequence yet, and a retained lease can have advanced its sequence,
/// so every header integer is counted at its maximum encoded width. Clamping
/// the unsigned run bounds at checkout can only make the frame smaller.
fn start_reservation(
    start: &lash_vm_protocol::Start,
    codec: &lash_vm_protocol::FrameCodec,
) -> Result<usize, PoolError> {
    use lash_vm_protocol::{
        BorrowedParentFrame, BorrowedParentMessage, ExecutionLease, MessageHeader,
        TransportSequence,
    };
    let bytes = codec.borrowed_frame_len(&BorrowedParentFrame {
        header: MessageHeader {
            lease: ExecutionLease(u64::MAX),
            owner_epoch: OwnerEpoch(u64::MAX),
            frame_epoch: FrameEpoch(u64::MAX),
            sequence: TransportSequence(u64::MAX),
        },
        message: BorrowedParentMessage::Start(start),
    })?;
    usize::try_from(bytes).map_err(|_| PoolError::QueueFull { bytes: usize::MAX })
}

#[cfg(test)]
mod reservation_tests {
    use super::*;
    use lash_vm_protocol::{
        ExecutionLease, MessageHeader, ParentFrame, ParentMessage, TransportSequence,
    };

    /// START-RESERVATION-IDENTITY: admission counts the complete encoded input,
    /// including its owner binding and the widest possible transport header.
    #[test]
    fn start_reservation_counts_the_complete_encoded_frame() {
        let codec = lash_vm_protocol::FrameCodec::new(lash_vm_protocol::DecodeLimits::standard());
        let start = lash_vm_protocol::Start {
            owner: VmOwner::new("reservation-law"),
            dialect: "typescript".to_owned(),
            document: EncodedPayload(vec![0; 16]),
            from: StartFrom::Fresh(EncodedPayload(vec![0; 16])),
            bounds: RunBounds {
                charge: u64::MAX,
                memory: u64::MAX,
                call_depth: u32::MAX,
                live_tasks: u32::MAX,
                requests_per_park: u32::MAX,
                join_members: u32::MAX,
            },
            binding: [7; 32],
            save: None,
            warm: Vec::new(),
            observe_sites: true,
            observe_counts: false,
            observe_path: false,
        };
        let actual = codec
            .encode_parent(&ParentFrame {
                header: MessageHeader {
                    lease: ExecutionLease(u64::MAX),
                    owner_epoch: OwnerEpoch(u64::MAX),
                    frame_epoch: FrameEpoch(u64::MAX),
                    sequence: TransportSequence(u64::MAX),
                },
                message: ParentMessage::Start(Box::new(start.clone())),
            })
            .unwrap();
        assert_eq!(start_reservation(&start, &codec).unwrap(), actual.len());
    }
}
