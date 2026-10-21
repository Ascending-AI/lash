//! A kernel run's machine in a pooled worker process, as the broker drives
//! it.
//!
//! The worker hosts one machine for one execution (kernel spec §9 rule 5).
//! The machine's host is synchronous, so each host read crosses to the
//! parent in the middle of a `run` exchange and is answered here by a
//! [`RunHost`]: the clock, the random source, and the provider registered
//! for a projection handle. What the run prints arrives with the exchange
//! that printed it.

use lash_protocol_types::engine_fault::{
    ExecutionLease, FrameEpoch, InfrastructureOutcome, OwnerEpoch, PayloadKind, TransportSequence,
};
use std::sync::Arc;

use lash_core_store::checkpoint_graph::NodeId;
use lash_kernel_doc::heap::PageGrant;
use lash_kernel_doc::{Datum, ErrorDatum, Handle, IdentifiedDocument, Timestamp};

use lash_kernel_state::paged::SessionRoot;
use lash_kernel_vm::{Bounds, EffectModes, End, Outcome, Target, WaitId};
use lash_vm_broker::ParentFault;
use lash_vm_broker::kernel::closure::Closure;
use lash_vm_broker::kernel::pages::{PageExchange, SharedPages};
use lash_vm_broker::kernel::{
    DrivenMachine, KernelFailure, MachinePark, MachineStep, Machines, ParkedState,
};
use lash_vm_protocol::page::{PageReply, SealBatch};
use lash_vm_protocol::{
    BoundReached, CheckpointStart, EncodedPayload, HostReadKind, LimitDecision, LimitedBound,
    RunBounds, StartFrom, VmOwner,
};
use lash_vocabulary::names::DocumentId;
use lash_vocabulary::names::EffectName;

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
    pub link: lash_vocabulary::content_ids::LinkId,
    pub instances: std::collections::BTreeMap<
        lash_vocabulary::content_ids::LinkId,
        lash_kernel_vm::interface::PreparedInstance,
    >,
    pub documents: std::collections::BTreeMap<
        lash_vocabulary::names::DocumentId,
        Arc<lash_kernel_doc::Document>,
    >,
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
    pub host: Vec<(lash_vocabulary::names::Name, Datum)>,
    /// How a session cell's end saves its functions, fresh or resumed;
    /// none for a run that saves none.
    pub save: Option<wire::SaveWire>,
}

/// The parent's decision at a worker run's memory bound (FIG-6768): what
/// [`lash_kernel_vm::MemoryLimiter`] decides for an embedded run, asked
/// across the worker boundary with one [`HostReadKind::Bound`] read. The
/// worker asks only once its exact recount still leaves no room, at an
/// exact point before the statement's effect; a run resumed from an
/// earlier seal asks again at the same point. A grown limit is saved
/// with the run at its next seal.
pub trait RunMemoryLimiter: Send + Sync {
    /// The run needs `used` bytes and may hold `limit`.
    fn limit_reached(&self, used: u64, limit: u64) -> LimitDecision {
        let _ = (used, limit);
        LimitDecision::Refuse
    }
}

/// The parent's decision at a worker run's charge bound (FIG-6768): what
/// [`lash_kernel_vm::ChargeLimiter`] decides for an embedded run, asked as
/// [`RunMemoryLimiter`] is.
pub trait RunChargeLimiter: Send + Sync {
    /// The run is charged `used`, the crossing unit included, and may be
    /// charged `limit`.
    fn limit_reached(&self, used: u64, limit: u64) -> LimitDecision {
        let _ = (used, limit);
        LimitDecision::Refuse
    }
}

/// What a run reads from its host, answered in the parent.
#[async_trait::async_trait]
pub trait RunHost: Send + Sync {
    /// What decides at the run's memory bound; none refuses there, with
    /// no exchange.
    fn memory_limiter(&self) -> Option<&dyn RunMemoryLimiter> {
        None
    }

    /// What decides at the run's charge bound; none refuses there, with
    /// no exchange.
    fn charge_limiter(&self) -> Option<&dyn RunChargeLimiter> {
        None
    }

    /// The run segment's observation option, independent of live delivery.
    fn observe(&self) -> lash_kernel_vm::ObservationMode {
        lash_kernel_vm::ObservationMode::Off
    }

    /// Exact provisional counts since the worker's previous frame.
    fn ran(&self, _delta: &lash_kernel_vm::SiteCounts) {}

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
    fn enter_site(&self, _at: &lash_vocabulary::sites::EffectId) {}

    /// The time, read once for each `clock` call the run makes.

    /// Sixty-four random bits.

    /// A read through a projection handle. `Err` is raised in the guest,
    /// which may catch it.
    async fn read(
        &self,
        handle: &Handle,
        request: &Datum,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault>;

    /// A `perform` the run binds as a read (`K-EFF-012`), at `site` under
    /// the authored statement `origin`: the effect's result, or the error
    /// raised at the `perform`, which the guest may catch. `Err` is the
    /// parent's own failure: the run stops there, unanswered, and its last
    /// save stands.
    async fn perform(
        &self,
        effect: &lash_vocabulary::names::OperationUse,
        args: &[Datum],
        site: &lash_vocabulary::sites::PositionLabel,
        origin: &lash_vocabulary::sites::Site,
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
    /// process recorded it: the worker refuses one its embedding lacks.
    dialect: String,
    /// The document's token body, as every checkout's `Start` carries it.
    body: Arc<[u8]>,
    start: RunStart,
    bounds: Bounds,
    /// The document's execution closure, as the parent derived it over the
    /// functions its node holds and the definitions of the others it read
    /// from the store, which every checkout's `Start` carries.
    closure: Closure,
    /// The closures of the session's earlier cells no commit installed.
    carried: Vec<Closure>,
}

impl RemoteMachines {
    /// The run of `document`, written in `dialect`, from `start`, held to
    /// `bounds`, its roots pinning `closure`, the document's execution
    /// closure.
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
        closure: Closure,
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
            closure,
            carried: Vec::new(),
        })
    }

    /// These machines, whose session's earlier cells ran under `carried`,
    /// closures no commit installed yet: a park whose pages name one
    /// stages it.
    #[must_use]
    pub fn with_carried(mut self, carried: Vec<Closure>) -> Self {
        self.carried = carried;
        self
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
            link: self.start.link,
            instances: wire::encode(
                PayloadKind::Start,
                &self.start.instances.values().collect::<Vec<_>>(),
            )
            .map_err(worker)?,
            documents: self
                .start
                .documents
                .iter()
                .map(
                    |(document, body)| lash_vm_protocol::message::StoredDocument {
                        document: *document,
                        body: EncodedPayload(lash_vocabulary::tokens::encode_body(body.as_ref())),
                    },
                )
                .collect(),
            functions: self.closure.body().identities().collect(),
            definitions: self
                .closure
                .stored()
                .iter()
                .map(|(function, body)| lash_vm_protocol::StoredDefinition {
                    function: *function.as_bytes(),
                    body: EncodedPayload(body.to_vec()),
                })
                .collect(),
            from,
            bounds: RunBounds {
                memory_meter: match self.bounds.memory_meter {
                    lash_kernel_vm::MemoryMeter::On => lash_vm_protocol::MemoryMeter::On,
                    lash_kernel_vm::MemoryMeter::Off => lash_vm_protocol::MemoryMeter::Off,
                },
                charge: match self.bounds.charge {
                    lash_kernel_vm::Charge::Exact => lash_vm_protocol::Charge::Exact,
                    lash_kernel_vm::Charge::Off => lash_vm_protocol::Charge::Off,
                },
                charge_limit: self.bounds.charge_limit,
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
            observe_counts: self.host.observe() == lash_kernel_vm::ObservationMode::Counts,
            observe_path: self.host.traces_path(),
            memory_limiter: self.host.memory_limiter().is_some(),
            charge_limiter: self.host.charge_limiter().is_some(),
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
            lash_kernel_vm::Target::Entry(_) | lash_kernel_vm::Target::Callable(_) => {
                ExecutionClass::Process
            }
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
            path: lash_kernel_vm::ExecutionPath::default(),
            caught: Vec::new(),
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

    fn closure(&self) -> Result<&Closure, KernelFailure> {
        Ok(&self.closure)
    }

    fn carried(&self) -> &[Closure] {
        &self.carried
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
                session
                    .pages()
                    .into_iter()
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
    path: lash_kernel_vm::ExecutionPath,
    caught: Vec<lash_kernel_vm::WaitId>,
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
    pub fn take_ran(&mut self) -> lash_kernel_vm::SiteCounts {
        std::mem::take(&mut self.ran).into_iter().collect()
    }

    /// Takes and resets the statement path received in this run segment's
    /// frames, in order, when the host traces it.
    pub fn take_path(&mut self) -> lash_kernel_vm::ExecutionPath {
        std::mem::take(&mut self.path)
    }

    /// Takes and resets the waits whose failed outcome a `catch` recovered
    /// from, as this run segment's park or end frame carried them
    /// (FIG-6817).
    pub fn take_caught(&mut self) -> Vec<lash_kernel_vm::WaitId> {
        std::mem::take(&mut self.caught)
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
        let carried: Vec<NodeId> = batch.pages.iter().map(|page| NodeId(page.id())).collect();
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
                PageLink::Carried(_) => {
                    Err(lash_protocol_types::engine_fault::PageRefusal::Unauthorized)
                }
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
                    let config = self.service.config();
                    // The worker waits on this answer holding its checkout:
                    // a host that outlasts its read deadline ends the run,
                    // and the waiting worker is discarded.
                    let bound = config.deadlines.host_read;
                    let answer = tokio::time::timeout(
                        bound,
                        answer(
                            host.as_ref(),
                            kind,
                            &request,
                            config.protocol.max_effect_value_bytes,
                        ),
                    )
                    .await;
                    if let (Some(scope), Some(start)) = (scope, started) {
                        scope.record("host_body", start);
                    }
                    let Ok(answer) = answer else {
                        self.checkout = None;
                        return Err(worker(PoolError::from(
                            InfrastructureOutcome::WorkerLimitExceeded {
                                limit: crate::config::host_read_limit(bound),
                            },
                        )));
                    };
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
            self.path.append(sites.path);
            self.caught.extend(sites.caught);
        }
        Ok(())
    }
}

/// The host's answer to one read, encoded within `bound`, the effect value
/// bound: an answer that passes it is refused while it is encoded.
async fn answer(
    host: &dyn RunHost,
    kind: HostReadKind,
    request: &EncodedPayload,
    bound: u64,
) -> Result<EncodedPayload, KernelFailure> {
    match kind {
        HostReadKind::Projection => {
            let read: ProjectionRead =
                wire::decode(PayloadKind::HostRead, request).map_err(worker)?;
            wire::encode_within(
                PayloadKind::HostAnswer,
                &host.read(&read.handle, &read.request).await?,
                bound,
            )
        }
        HostReadKind::Effect => {
            let read: EffectRead = wire::decode(PayloadKind::HostRead, request).map_err(worker)?;
            wire::encode_within(
                PayloadKind::HostAnswer,
                &host
                    .perform(&read.use_, &read.args, &read.site, &read.origin)
                    .await?,
                bound,
            )
        }
        HostReadKind::Bound => {
            let read: BoundReached =
                wire::decode(PayloadKind::HostRead, request).map_err(worker)?;
            let decision = match read.bound {
                LimitedBound::Memory => host
                    .memory_limiter()
                    .map(|limiter| limiter.limit_reached(read.used, read.limit)),
                LimitedBound::Charge => host
                    .charge_limiter()
                    .map(|limiter| limiter.limit_reached(read.used, read.limit)),
            };
            wire::encode(PayloadKind::HostAnswer, &decision.unwrap_or_default())
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
            Stop::Sealed(_) => Err(unexpected(
                lash_protocol_types::worker_limit::WorkerFrameKind::Sealed,
            )),
        }
    }

    /// The outcome is encoded within the effect value bound: one that
    /// passes it is refused while it is encoded, before the parent holds
    /// its whole encoding. The checkout checks the bound again as it
    /// queues the delivery.
    async fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<(), KernelFailure> {
        self.candidate = None;
        let outcome = wire::encode_within(
            PayloadKind::Outcome,
            &OutcomeWire::from(outcome),
            self.service.config().protocol.max_effect_value_bytes,
        )
        .map_err(worker)?;
        self.with(move |checkout| checkout.deliver(wait, outcome))
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
            Stop::Parked { .. } => Err(unexpected(
                lash_protocol_types::worker_limit::WorkerFrameKind::Parked,
            )),
            Stop::Slice => Err(unexpected(
                lash_protocol_types::worker_limit::WorkerFrameKind::Slice,
            )),
            Stop::Ended { .. } => Err(unexpected(
                lash_protocol_types::worker_limit::WorkerFrameKind::Ended,
            )),
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

fn unexpected(found: lash_protocol_types::worker_limit::WorkerFrameKind) -> KernelFailure {
    worker(PoolError::breach(
        lash_protocol_types::engine_fault::ProtocolBreach::Unexpected {
            exchange: lash_protocol_types::engine_fault::Exchange::Seal,
            found,
        },
    ))
}

fn refused(refusal: lash_protocol_types::engine_fault::PageRefusal) -> KernelFailure {
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
                lash_protocol_types::engine_fault::ProtocolBreach::Panicked {
                    detail: lash_protocol_types::engine_fault::Detail::new(error),
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
    use lash_vm_protocol::{BorrowedParentFrame, BorrowedParentMessage, MessageHeader};
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
    use lash_vm_protocol::{MessageHeader, ParentFrame, ParentMessage};

    /// START-RESERVATION-IDENTITY: admission counts the complete encoded input,
    /// including its owner binding and the widest possible transport header.
    #[test]
    fn start_reservation_counts_the_complete_encoded_frame() {
        let codec = lash_vm_protocol::FrameCodec::new(lash_vm_protocol::DecodeLimits::standard());
        let start = lash_vm_protocol::Start {
            owner: VmOwner::new("reservation-law"),
            dialect: "typescript".to_owned(),
            document: EncodedPayload(vec![0; 16]),
            documents: Vec::new(),
            instances: EncodedPayload(Vec::new()),
            link: lash_vocabulary::LinkId::from_bytes([1; 32]),
            definitions: Vec::new(),
            functions: Default::default(),
            from: StartFrom::Fresh(EncodedPayload(vec![0; 16])),
            bounds: RunBounds {
                memory_meter: lash_vm_protocol::MemoryMeter::On,
                charge: lash_vm_protocol::Charge::Exact,
                charge_limit: u64::MAX,
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
            memory_limiter: false,
            charge_limiter: false,
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
