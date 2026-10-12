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

use lash_core_store::checkpoint_graph::bodies::OwnerBinding;
use lash_core_store::checkpoint_graph::{DecodeLimits, NodeKind};
use lash_kernel_doc::{Datum, Document, DocumentId, ErrorDatum, Handle, KernelVersion, Timestamp};
use lash_kernel_vm::{Bounds, End, Outcome, Start, Step, WaitId};
use lash_sansio::VersionRange;
use lash_vm_broker::ParentFault;
use lash_vm_broker::kernel::state::{GraphStateFailure, ReleaseGraph, WholeState};
use lash_vm_broker::kernel::{DrivenMachine, KernelFailure, Machines, ParkedState};
use lash_vm_protocol::{
    EncodedPayload, FrameEpoch, GraphStateExpectation, HostReadKind, OwnerEpoch, PayloadKind,
    RunBounds, StartFrom, VmOwner,
};

use crate::service::{CallHeld, Service};
use crate::wire::{self, EndWire, OutcomeWire, ParkWire, ProjectionRead, StartWire};
use crate::{Checkout, ExecutionClass, PoolError, RunStep, WorkerPoolRuntimeOps as _};

/// The kernel versions this build's workers resume a parked run under.
pub fn kernel_reads() -> VersionRange {
    let newest = KernelVersion::NEWEST.number();
    let oldest = KernelVersion::ALL
        .iter()
        .map(|version| version.number())
        .min()
        .unwrap_or(newest);
    VersionRange::between(oldest, newest)
}

/// What a run reads from its host, answered in the parent.
#[async_trait::async_trait]
pub trait RunHost: Send + Sync {
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
    encoded: Arc<Vec<u8>>,
    start: Start,
    bounds: Bounds,
    releases: std::sync::OnceLock<Result<ReleaseGraph, GraphStateFailure>>,
}

impl RemoteMachines {
    /// The run of `document`, written in `dialect`, from `start`, held to
    /// `bounds`. `dialect` is recorded configuration: what the run's session
    /// or process recorded at creation, never a live read.
    ///
    /// # Errors
    ///
    /// [`KernelFailure::Document`] when the document has no canonical
    /// encoding.
    #[expect(clippy::too_many_arguments, reason = "one run's whole identity")]
    pub fn new(
        service: Service,
        owner: VmOwner,
        owner_epoch: OwnerEpoch,
        frame_epoch: FrameEpoch,
        host: Arc<dyn RunHost>,
        document: &Document,
        dialect: &str,
        start: Start,
        bounds: Bounds,
    ) -> Result<Self, KernelFailure> {
        Ok(Self {
            service,
            owner,
            owner_epoch,
            frame_epoch,
            host,
            document: document.identity()?,
            dialect: dialect.to_owned(),
            encoded: Arc::new(
                wire::wrap(PayloadKind::Document, document.to_json()?.as_bytes())
                    .map_err(worker)?
                    .0,
            ),
            start,
            bounds,
            releases: std::sync::OnceLock::new(),
        })
    }

    /// A checkout holding the run: the one that lowered its document when
    /// the execution kept it, and otherwise one from the pool. Its state at
    /// each park that requests something is bound to `binding`.
    async fn checkout(
        &self,
        from: StartFrom,
        binding: &OwnerBinding,
    ) -> Result<RemoteMachine, KernelFailure> {
        let start = lash_vm_protocol::Start {
            owner: self.owner.clone(),
            dialect: self.dialect.clone(),
            document: EncodedPayload(self.encoded.as_ref().clone()),
            from,
            bounds: RunBounds {
                charge: self.bounds.charge,
                memory: self.bounds.memory,
                call_depth: self.bounds.call_depth,
                live_tasks: self.bounds.live_tasks,
                requests_per_park: self.bounds.requests_per_park,
                join_members: self.bounds.join_members,
            },
            export: Some(EncodedPayload(lash_kernel_doc::graph_tokens::encode_body(
                binding,
            ))),
        };
        let reservation = start.document.0.len()
            + match &start.from {
                StartFrom::Fresh(payload) => payload.0.len(),
                StartFrom::ParkedGraph(parked) => {
                    parked.state.byte_len() as usize
                        + parked
                            .admissions
                            .iter()
                            .map(|admission| admission.0.len())
                            .sum::<usize>()
                }
            }
            + 4096;
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
            owner: self.owner.clone(),
            document: self.document.to_string(),
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

    async fn start(&self, binding: &OwnerBinding) -> Result<RemoteMachine, KernelFailure> {
        let start = wire::encode(PayloadKind::Start, &StartWire::from(self.start.clone()))
            .map_err(worker)?;
        self.checkout(StartFrom::Fresh(start), binding).await
    }

    fn releases(&self) -> Result<&ReleaseGraph, KernelFailure> {
        self.releases
            .get_or_init(lash_vm_broker::kernel::state::release_manifests)
            .as_ref()
            .map_err(|failure| KernelFailure::State(failure.clone()))
    }

    /// Resumes a parked run from its checkpoint graph state and the
    /// admission of each wait it holds (FIG-5887 §3).
    async fn resume(
        &self,
        parked: &ParkedState,
        binding: &OwnerBinding,
    ) -> Result<RemoteMachine, KernelFailure> {
        self.resume_sealed(parked.sealed(self.owner.clone()), binding)
            .await
    }
}

impl RemoteMachines {
    /// Resumes a parked run from sealed graph state and the admission of
    /// each wait it holds: a holder that keeps the state sealed, such as a
    /// kernel process's engine. Its state at each park that requests
    /// something is bound to `binding`.
    ///
    /// # Errors
    ///
    /// The worker's refusal of the state, its release or an admission.
    pub async fn resume_sealed(
        &self,
        parked: lash_vm_protocol::ParkedGraph,
        binding: &OwnerBinding,
    ) -> Result<RemoteMachine, KernelFailure> {
        self.checkout(StartFrom::ParkedGraph(parked), binding).await
    }

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
    owner: VmOwner,
    document: String,
}

impl RemoteMachine {
    /// The run's end, as the worker sealed it: refused unless it is this
    /// owner's TerminalWhole under the run's document.
    fn end(&self, end: EndWire) -> Result<End, KernelFailure> {
        let Some(state) = end else {
            return Ok(End::Cancelled);
        };
        state
            .check(&GraphStateExpectation {
                owner: &self.owner,
                kernel: kernel_reads(),
                document: Some(&self.document),
                root_kinds: &[NodeKind::TERMINAL_WHOLE.0],
                max_nodes: u64::MAX,
                max_bytes: u64::MAX,
            })
            .map_err(|refusal| {
                worker(PoolError::from(
                    lash_vm_protocol::InfrastructureOutcome::output_graph_state(refusal),
                ))
            })?;
        lash_vm_broker::kernel::state::read_end(
            &state,
            self.service.config().graph_reads.execution(),
        )
        .map_err(KernelFailure::State)
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
    }
    .map_err(worker)
}

#[async_trait::async_trait]
impl DrivenMachine for RemoteMachine {
    async fn run(&mut self, slice: u64, cancel: bool) -> Result<Step, KernelFailure> {
        let mut step = self
            .with(move |checkout| checkout.run(slice, cancel))
            .await?;
        loop {
            self.printed()?;
            match step {
                RunStep::HostRead { id, kind, request } => {
                    let host = Arc::clone(&self.host);
                    let answer = answer(host.as_ref(), kind, &request).await?;
                    step = self
                        .with(move |checkout| checkout.host_answer(id, answer))
                        .await?;
                }
                RunStep::Parked { park, .. } => {
                    let park: ParkWire = wire::decode(PayloadKind::Park, &park).map_err(worker)?;
                    return Ok(Step::Parked(park.try_into().map_err(worker)?));
                }
                RunStep::Slice { .. } => return Ok(Step::Slice),
                RunStep::Ended { end, .. } => {
                    let end: EndWire = wire::decode(PayloadKind::End, &end).map_err(worker)?;
                    return Ok(Step::Ended(self.end(end)?));
                }
            }
        }
    }

    async fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<(), KernelFailure> {
        let outcome =
            wire::encode(PayloadKind::Outcome, &OutcomeWire::from(outcome)).map_err(worker)?;
        self.with(move |checkout| checkout.deliver(wait.0, outcome))
            .await
    }

    /// The run's state at this park as checkpoint graph state, bound to the
    /// execution `binding` names (FIG-5887 §3): sealed by the worker,
    /// checked by the pool, and each node checked against its digest here.
    async fn export(&mut self, binding: &OwnerBinding) -> Result<WholeState, KernelFailure> {
        let binding = EncodedPayload(lash_kernel_doc::graph_tokens::encode_body(binding));
        let state = self
            .with(move |checkout| checkout.export_graph(binding))
            .await?;
        WholeState::of_sealed(&state, &DecodeLimits::within_total(state.byte_len()))
            .map_err(|refusal| KernelFailure::State(GraphStateFailure::Graph(refusal)))
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
