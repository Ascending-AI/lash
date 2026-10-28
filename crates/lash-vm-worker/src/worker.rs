use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use crate::PoolError;
use crate::embedding::Embedding;
use crate::host::{PagePort, Wire, WireHost, WireLimiter, WirePageSource, grant_of, link_grant};
use lash_kernel_doc::Datum;
use lash_kernel_doc::heap::{PageId, PageRef};
use lash_vm_client::packages::WorkerLibraryRuntimeOps as _;

use lash_kernel_state::paged::{Checkpoint, PageSource};
use lash_kernel_vm::{Bounds, CellStart, KernelMachine, Machine, OpenError, Program, Step};
use lash_protocol_types::engine_fault::{
    CheckpointRefusal, CodecRefusal, Detail, ExecutionLease, FrameEpoch, InfrastructureOutcome,
    KernelRestoreRefusal, OwnerEpoch, PayloadKind, ProtocolBreach, RunInput, RunRefusal,
    SequenceFault,
};
use lash_protocol_types::worker_limit::WorkerLimit;
#[cfg(test)]
use lash_vm_client::ipc::read_frame;
use lash_vm_client::ipc::{Bootstrap, FrameSource, write_frame, write_frames};
use lash_vm_client::wire::{self, EndWire, OutcomeWire, ParkWire, StartWire};
use lash_vm_protocol::*;

/// The one run a worker hosts between resets.
struct Hosted {
    machine: KernelMachine,
    /// The run's heap bound, which also bounds what one slice may print
    /// (FIG-4458).
    memory: u64,
    ended: bool,
    /// The root the run's last park sealed, until a delivery or a slice
    /// moves the run on: a `Seal` there answers with it.
    sealed: Option<[u8; 32]>,
    /// The run's page port: what the run hands the parent crosses first.
    port: PagePort,
    /// Whether the parent observes the document sites the run enters.
    observe_sites: bool,
    /// The document's site ordinals, when the parent records the run's
    /// statement path.
    path: Option<std::collections::BTreeMap<lash_kernel_value::sites::Site, u32>>,
}

struct WorkerContent {
    document: lash_kernel_doc::IdentifiedDocument,
    indexed: std::collections::BTreeMap<
        lash_kernel_value::LinkId,
        lash_kernel_vm::interface::PreparedInstance,
    >,
    documents:
        std::collections::BTreeMap<lash_kernel_value::DocumentId, Arc<lash_kernel_doc::Document>>,
}

pub(crate) struct Server<'embedding> {
    embedding: &'embedding Embedding,
    pipe: UnixStream,
    /// The parent's frames, shared with the run's host reads.
    inbound: Arc<Mutex<FrameSource>>,
    /// The run's host wire, made at its start and reused by every slice
    /// (FIG-4433).
    wire: Option<Arc<Wire>>,
    codec: FrameCodec,
    bootstrap: Bootstrap,
    hosted: Option<Hosted>,
    fences: Arc<Mutex<Fences>>,
    owner: Option<VmOwner>,
    /// Whether the lease's one `Prepare` crossed, and no run started since.
    prepared: bool,
    cpu_ceiling: Option<libc::rlim_t>,
}

pub(crate) struct Fences {
    pub incoming: Option<MessageFence>,
    pub outgoing: MessageFence,
    pub next_read: u64,
    pub next_page: u64,
}

impl Fences {
    pub(crate) fn encode(
        &mut self,
        codec: &FrameCodec,
        message: WorkerMessage,
    ) -> Result<Vec<u8>, PoolError> {
        let bytes = encode_worker(codec, self.outgoing.next_header_copy(), message).1?;
        self.outgoing.next_header();
        Ok(bytes)
    }
}

/// The frame's bytes, and its message back for the sender to keep.
fn encode_worker(
    codec: &FrameCodec,
    header: MessageHeader,
    message: WorkerMessage,
) -> (WorkerMessage, Result<Vec<u8>, PoolError>) {
    let kind = message.kind();
    let frame = WorkerFrame { header, message };
    let bytes = codec
        .encode_worker(&frame)
        .map_err(|refusal| match refusal {
            CodecRefusal::FrameTooLarge { limit, declared } => {
                InfrastructureOutcome::WorkerLimitExceeded {
                    limit: WorkerLimit::Frame {
                        kind,
                        size: declared,
                        bound: limit,
                    },
                }
                .into()
            }
            refusal => refusal.into(),
        });
    if let Ok(bytes) = &bytes {
        crate::perf_witness::buffer(bytes.capacity());
    }
    (frame.message, bytes)
}

impl<'embedding> Server<'embedding> {
    pub(crate) fn new(
        pipe: UnixStream,
        codec: FrameCodec,
        bootstrap: Bootstrap,
        embedding: &'embedding Embedding,
    ) -> Result<Self, PoolError> {
        // A ready worker runs its first document over a library compiled
        // here, before the parent hands it one: its first turn costs what a
        // warm one does (FIG-5922).
        embedding
            .prepared_library()
            .map_err(|error| machine_breach(error.to_string()))?;
        let tuning = bootstrap.tuning;
        Ok(Self {
            embedding,
            pipe,
            inbound: Arc::new(Mutex::new(FrameSource::with_capacity(
                tuning.inbound_buffer_bytes,
            ))),
            wire: None,
            codec,
            bootstrap,
            hosted: None,
            fences: Arc::new(Mutex::new(Fences {
                incoming: None,
                outgoing: MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0)),
                next_read: 0,
                next_page: 0,
            })),
            owner: None,
            prepared: false,
            cpu_ceiling: None,
        })
    }

    /// A worker is pooled only after its registry and preparation are ready.
    pub(crate) fn installed(&mut self) -> Result<(), PoolError> {
        let reply = lash_vm_client::packages::result(Ok(()))?;
        let bytes = self.codec.encode_pool_started(&reply)?;
        write_frame(
            &mut self.pipe,
            &bytes,
            Instant::now() + self.bootstrap.serialization,
        )
    }

    fn send(&mut self, message: WorkerMessage) -> Result<(), PoolError> {
        let bytes = self
            .fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .encode(&self.codec, message)?;
        write_frame(
            &mut self.pipe,
            &bytes,
            Instant::now() + self.bootstrap.serialization,
        )
    }
    /// Tells the parent why the worker stops, where that is the worker's to
    /// say. A lost pipe is the parent's own evidence, and nothing is sent.
    pub(crate) fn refuse(&mut self, error: &PoolError) -> Result<(), PoolError> {
        match lash_vm_protocol::outcome::testimony(error.clone().into_outcome()) {
            Some(message) => self.send(message),
            None => Ok(()),
        }
    }
    fn run_meters(&self) -> Option<RunMeters> {
        self.hosted.as_ref().map(|hosted| {
            let meters = hosted.machine.meters();
            RunMeters {
                charged: meters.charged,
                memory: meters.memory,
                live_tasks: meters.live_tasks,
            }
        })
    }
    fn progress(&mut self, phase: WorkerPhase) -> Result<(), PoolError> {
        if phase == WorkerPhase::Computing && self.cpu_ceiling.is_none() {
            let nanos = u128::from(cpu_nanos()?) + u128::from(self.bootstrap.cpu_nanos);
            let seconds = nanos
                .div_ceil(1_000_000_000)
                .try_into()
                .map_err(|_| PoolError::InvalidConfiguration)?;
            let limit = libc::rlimit {
                rlim_cur: seconds,
                rlim_max: libc::RLIM_INFINITY,
            };
            #[expect(
                unsafe_code,
                reason = "the worker sets its own CPU ceiling before guest work; it survives parent loss"
            )]
            let result = unsafe { libc::setrlimit(libc::RLIMIT_CPU, &limit) };
            if result != 0 {
                return Err(PoolError::io(std::io::Error::last_os_error()));
            }
            self.cpu_ceiling = Some(seconds);
        }
        self.send(WorkerMessage::Progress {
            phase,
            cpu_nanos: cpu_nanos()?,
            meters: self.run_meters(),
            work: None,
        })
    }
    pub(crate) fn run(
        &mut self,
        hook: &mut Option<&mut dyn FnMut(&ParentMessage)>,
    ) -> Result<(), PoolError> {
        loop {
            // Host waits have no execution deadline. EOF terminates the entry.
            let received = self
                .inbound
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .read_frame(
                    &mut self.pipe,
                    &self.codec,
                    Instant::now() + self.bootstrap.tuning.parent_wait,
                );
            let bytes = match received {
                Ok(bytes) => bytes,
                Err(PoolError::Infrastructure(InfrastructureOutcome::WorkerCrashed { .. })) => {
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            crate::perf_witness::buffer(
                bytes.capacity()
                    + self
                        .inbound
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .buffer_capacity(),
            );
            let frame = self.codec.decode_parent(&bytes).map_err(PoolError::from)?;
            let mut fences = self
                .fences
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if fences.incoming.is_none() {
                if !matches!(
                    frame.message,
                    ParentMessage::Start(_)
                        | ParentMessage::Run { start: Some(_), .. }
                        | ParentMessage::Prepare { .. }
                        | ParentMessage::Reset
                        | ParentMessage::Cancel
                        | ParentMessage::Shutdown
                ) {
                    return Err(PoolError::breach(SequenceFault::IdleWithoutStart));
                }
                fences.incoming = Some(MessageFence::new(
                    frame.header.lease,
                    frame.header.owner_epoch,
                    frame.header.frame_epoch,
                ));
                fences.outgoing = MessageFence::new(
                    frame.header.lease,
                    frame.header.owner_epoch,
                    frame.header.frame_epoch,
                );
            }
            fences
                .incoming
                .as_mut()
                .ok_or_else(|| PoolError::breach(SequenceFault::MissingLease))?
                .admit(&frame.header)
                .map_err(PoolError::breach)?;
            drop(fences);
            if let Some(hook) = hook.as_mut() {
                hook(&frame.message);
            }
            match frame.message {
                ParentMessage::Start(start) => {
                    self.admit_start(&start)?;
                    self.progress(WorkerPhase::Computing)?;
                    #[cfg(feature = "cell-memory")]
                    crate::cell_memory::worker_begin().map_err(PoolError::io)?;
                    self.hosted = Some(self.start(*start)?);
                    self.progress(WorkerPhase::Serializing)?;
                    self.respond(WorkerMessage::Started)?;
                }
                ParentMessage::Prepare { owner, request } => {
                    if self.owner.is_some() {
                        return Err(PoolError::breach(SequenceFault::PrepareBeforeReset));
                    }
                    self.owner = Some(owner);
                    self.prepared = true;
                    self.progress(WorkerPhase::Computing)?;
                    self.codec.check_payload(&request.0)?;
                    let response = crate::service::perform(self.embedding, &request)?;
                    self.progress(WorkerPhase::Serializing)?;
                    self.respond(WorkerMessage::Prepared { response })?;
                }
                ParentMessage::Run {
                    start,
                    deliveries,
                    slice,
                    cancel,
                    sealed,
                } => {
                    if let Some(start) = &start {
                        self.admit_start(start)?;
                    }
                    self.progress(WorkerPhase::Computing)?;
                    if let Some(start) = start {
                        #[cfg(feature = "cell-memory")]
                        crate::cell_memory::worker_begin().map_err(PoolError::io)?;
                        self.hosted = Some(self.start(*start)?);
                    }
                    // The grants for the batch that crossed with the last
                    // hand-over, before anything reads or references it.
                    self.running()?.port.grant(sealed).map_err(heap_fault)?;
                    for delivery in deliveries {
                        self.deliver(delivery)?;
                    }
                    let message = match self.slice(slice, cancel) {
                        Err(PoolError::Infrastructure(
                            InfrastructureOutcome::WorkerLimitExceeded { limit },
                        )) => WorkerMessage::LimitExceeded { limit },
                        result => result?,
                    };
                    self.respond(message)?;
                }
                ParentMessage::Deliver { wait, outcome } => {
                    self.progress(WorkerPhase::Computing)?;
                    self.deliver(Delivery { wait, outcome })?;
                    self.progress(WorkerPhase::Serializing)?;
                    self.respond(WorkerMessage::Delivered)?;
                }
                ParentMessage::Seal => {
                    self.progress(WorkerPhase::Computing)?;
                    let message = match self.seal() {
                        Err(PoolError::Infrastructure(
                            InfrastructureOutcome::WorkerLimitExceeded { limit },
                        )) => WorkerMessage::LimitExceeded { limit },
                        result => {
                            let candidate = result?;
                            WorkerMessage::Sealed {
                                seal: self.running()?.port.take_batch(),
                                candidate,
                            }
                        }
                    };
                    self.progress(WorkerPhase::Serializing)?;
                    self.respond(message)?;
                }
                ParentMessage::HostAnswer { .. } => {
                    // A read is answered inside the slice that made it.
                    return Err(PoolError::breach(SequenceFault::NoPendingRead));
                }
                ParentMessage::Documents { .. } | ParentMessage::Pages { .. } => {
                    // So is a page exchange.
                    return Err(PoolError::breach(SequenceFault::NoPendingPages));
                }
                ParentMessage::Cancel => {
                    // Physical cancellation never decides the durable winner.
                    self.hosted = None;
                    self.wire = None;
                    self.send(WorkerMessage::Cancelled)?;
                }
                ParentMessage::Reset => {
                    self.cpu_ceiling = None;
                    self.hosted = None;
                    self.wire = None;
                    self.owner = None;
                    self.prepared = false;
                    #[cfg(feature = "dhat-heap")]
                    crate::heap_profile::finish().map_err(PoolError::io)?;
                    self.fences
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .next_read = 0;
                    self.fences
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .next_page = 0;
                    self.send(WorkerMessage::ResetDone {
                        cpu_nanos: cpu_nanos()?,
                    })?;
                    self.fences
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .incoming = None;
                }
                ParentMessage::Shutdown => return Ok(()),
            }
        }
    }
    /// Admits the lease's one run: on a worker that served nothing under
    /// the lease, or right after the lease's `Prepare`. A reset between the
    /// two would drop no machine and no wire; it would drop the owner and
    /// the CPU ceiling, so the run takes its own owner and its ceiling
    /// counts from its own start, as after a reset.
    fn admit_start(&mut self, start: &Start) -> Result<(), PoolError> {
        if self.owner.is_some() && !self.prepared {
            return Err(PoolError::breach(SequenceFault::StartBeforeReset));
        }
        if std::mem::take(&mut self.prepared) {
            self.cpu_ceiling = None;
        }
        self.owner = Some(start.owner.clone());
        Ok(())
    }
    /// Hands the run one committed outcome.
    fn deliver(&mut self, delivery: Delivery) -> Result<(), PoolError> {
        let outcome: OutcomeWire = self.decode(PayloadKind::Outcome, &delivery.outcome)?;
        let hosted = self.running()?;
        #[cfg(feature = "cell-memory")]
        crate::cell_memory::attach(&mut hosted.machine);
        hosted.sealed = None;
        hosted
            .machine
            .deliver(delivery.wait, outcome.into())
            .map_err(machine_breach)?;
        Ok(())
    }
    /// Seals the run as it stands at this safe point: its changed pages and
    /// a checkpoint root, which cross to the parent's staging with the
    /// frame that hands the run over. Returns the root's page.
    fn seal(&mut self) -> Result<[u8; 32], PoolError> {
        let hosted = self.running()?;
        if let Some(sealed) = hosted.sealed {
            return Ok(sealed);
        }
        let root = hosted.machine.checkpoint().map_err(|error| match error {
            lash_kernel_vm::SealError::Heap(fault) => heap_fault(fault),
            other => machine_breach(other),
        })?;
        hosted.port.checkpointed(root.page).map_err(heap_fault)?;
        let candidate = root.page.id.0;
        hosted.sealed = Some(candidate);
        Ok(candidate)
    }
    /// The run's host wire, on this socket and under this run's fences.
    fn wire(&mut self) -> Result<Arc<Wire>, PoolError> {
        if let Some(wire) = &self.wire {
            return Ok(wire.clone());
        }
        let wire = Arc::new(Wire::new(
            self.pipe.try_clone().map_err(PoolError::io)?,
            self.inbound.clone(),
            self.codec.clone(),
            self.fences.clone(),
            self.bootstrap.serialization,
            self.bootstrap.tuning.parent_wait,
            self.bootstrap.effect,
        ));
        self.wire = Some(wire.clone());
        Ok(wire)
    }
    fn decode<T: serde::de::DeserializeOwned>(
        &self,
        kind: PayloadKind,
        payload: &EncodedPayload,
    ) -> Result<T, PoolError> {
        if payload.0.len() as u64 > self.bootstrap.effect {
            return Err(InfrastructureOutcome::WorkerLimitExceeded {
                limit: WorkerLimit::EffectValue {
                    size: payload.0.len() as u64,
                    bound: self.bootstrap.effect,
                },
            }
            .into());
        }
        self.codec.check_payload(&payload.0)?;
        wire::decode(kind, payload)
    }
    /// The run a `Start` put here, while it has not ended.
    fn running(&mut self) -> Result<&mut Hosted, PoolError> {
        match self.hosted.as_mut() {
            None => Err(PoolError::breach(SequenceFault::NoRun)),
            Some(hosted) if hosted.ended => Err(PoolError::breach(SequenceFault::RunEnded)),
            Some(hosted) => Ok(hosted),
        }
    }
    /// A machine for the run: one that has executed nothing, or one opened
    /// on a committed checkpoint root, its heap read and sealed through the
    /// parent's page service. The document is validated and compiled
    /// against the registry this worker assembled when it started, with
    /// each function redeclared for the document's kernel version when that
    /// is a successor's; nothing is loaded from the document.
    fn start(&mut self, start: Start) -> Result<Hosted, PoolError> {
        self.wire = None;
        // The run's dialect is what its session or process recorded at
        // creation. Running a document needs no dialect: only a cell that
        // saves its functions writes them as the dialect's values, so a
        // worker whose host did not name the dialect refuses such a run
        // before it reads the document or the checkpoint (FIG-5990), and
        // runs any other (FIG-6985).
        if start.save.is_some() && !self.embedding.installs(&start.dialect) {
            return Err(PoolError::refused(RunRefusal::DialectNotInstalled {
                dialect: start.dialect,
            }));
        }
        let undecodable = |input: RunInput, error: &dyn std::fmt::Display| {
            PoolError::refused(RunRefusal::Undecodable {
                input,
                detail: Detail::new(error),
            })
        };
        let fresh = matches!(start.from, StartFrom::Fresh(_));
        let content = self.content(&start)?;
        let bounds = Bounds {
            memory_meter: match start.bounds.memory_meter {
                lash_vm_protocol::MemoryMeter::On => lash_kernel_vm::MemoryMeter::On,
                lash_vm_protocol::MemoryMeter::Off => lash_kernel_vm::MemoryMeter::Off,
            },
            charge: match start.bounds.charge {
                lash_vm_protocol::Charge::Exact => lash_kernel_vm::Charge::Exact,
                lash_vm_protocol::Charge::Off => lash_kernel_vm::Charge::Off,
            },
            charge_limit: start.bounds.charge_limit,
            memory: start.bounds.memory,
            call_depth: start.bounds.call_depth,
            live_tasks: start.bounds.live_tasks,
            requests_per_park: start.bounds.requests_per_park,
            join_members: start.bounds.join_members,
        };
        let owner = PageId(start.binding);
        let mut source = WirePageSource::new(self.wire()?, self.bootstrap.tuning.pages.clean_bytes);
        let port = source.port();
        port.warm(&start.warm).map_err(heap_fault)?;
        let checkpoint = match &start.from {
            StartFrom::Fresh(_) => None,
            StartFrom::Checkpoint(checkpoint) => {
                let page = PageRef {
                    id: PageId(checkpoint.root.page),
                    grant: grant_of(&checkpoint.root),
                };
                let answers = source.read(&[page]).map_err(heap_fault)?;
                let answer = answers
                    .first()
                    .ok_or_else(|| undecodable(RunInput::Checkpoint, &"the root was not read"))?;
                let answer = answer.as_ref().map_err(|error| heap_fault(error.clone()))?;
                let root = Checkpoint::of_answer(answer).map_err(|fault| {
                    PoolError::refused(RunRefusal::Checkpoint {
                        refusal: CheckpointRefusal::Malformed {
                            problem: fault.to_string(),
                        },
                    })
                })?;
                if root.header().owner != owner {
                    return Err(PoolError::refused(RunRefusal::Checkpoint {
                        refusal: CheckpointRefusal::WrongOwner {
                            expected: hex(&owner.0),
                            found: hex(&root.header().owner.0),
                        },
                    }));
                }
                Some(root)
            }
        };
        let selection = match (&checkpoint, &start.from) {
            (Some(root), StartFrom::Checkpoint(checkpoint))
                if root.header().helper_library
                    != self.embedding.library_image().helpers().identity() =>
            {
                let payload = checkpoint.selection.as_ref().ok_or_else(|| {
                    PoolError::refused(RunRefusal::Checkpoint {
                        refusal: CheckpointRefusal::Malformed {
                            problem:
                                "a changed helper library requires the parent's live selection"
                                    .into(),
                        },
                    })
                })?;
                self.codec.check_payload(&payload.0)?;
                let selection: lash_kernel_doc::RestoreSelection =
                    wire::decode(PayloadKind::Start, payload)?;
                lash_kernel_state::paged::check_helper_compatibility(
                    root.header().helper_library,
                    || Ok(lash_kernel_state::paged::HelperPins(selection.pins.clone())),
                    self.embedding.library_image().helpers(),
                )
                .map_err(helper_compatibility_refusal)?;
                lash_kernel_doc::Selection::Restored(selection)
            }
            _ => lash_kernel_doc::Selection::Fresh,
        };
        let source: Box<dyn PageSource> = Box::new(source);
        let program = self.program(&start, fresh, content, selection)?;
        let path = start.observe_path.then(|| program.document.site_ordinals());
        let save = start
            .save
            .as_ref()
            .map(|payload| {
                self.codec.check_payload(&payload.0)?;
                let save: wire::SaveWire = wire::decode(PayloadKind::Start, payload)
                    .map_err(|error| undecodable(RunInput::Start, &error))?;
                Ok::<_, PoolError>(crate::save::DialectSave {
                    saved_written: save.saved_written,
                    values: self
                        .embedding
                        .dialects
                        .get(&start.dialect)
                        .and_then(|package| package.function_values.clone()),
                    controls: save.controls,
                    written: save.written,
                })
            })
            .transpose()?;
        let function_origins = save
            .as_ref()
            .map(|save| lash_kernel_vm::interface::FunctionOrigins {
                sites: save
                    .written
                    .iter()
                    .map(|(site, written)| ((program.link, site.clone()), written.clone()))
                    .collect(),
                environments: save.saved_written.clone(),
            })
            .unwrap_or_default();
        let captions = save
            .as_ref()
            .map(|save| {
                let mut captions = std::collections::BTreeMap::new();
                for document in program.documents.values() {
                    for entry in lash_kernel_dialect::function_captions(document, &save.written) {
                        captions.insert(entry.target.clone(), entry);
                    }
                }
                captions.into_values().collect()
            })
            .unwrap_or_default();
        let mut machine = match start.from {
            StartFrom::Fresh(payload) => {
                self.codec.check_payload(&payload.0)?;
                let from: StartWire = wire::decode(PayloadKind::Start, &payload)
                    .map_err(|error| undecodable(RunInput::Start, &error))?;
                let session = from
                    .session
                    .as_ref()
                    .map(|carry| carry.session(link_grant))
                    .transpose()
                    .map_err(|error| undecodable(RunInput::Start, &error))?;
                KernelMachine::start_with_data_limits(
                    program,
                    bounds,
                    CellStart {
                        owner,
                        target: from.target(),
                        modes: from.modes(),
                        args: from.args,
                        session,
                        host: from.host,
                    },
                    self.bootstrap.tuning.kernel_data,
                    source,
                )
                .map_err(|error| match error {
                    lash_kernel_vm::StartError::Heap(fault) => heap_fault(fault),
                    error @ lash_kernel_vm::StartError::Accounting { .. } => machine_breach(error),
                    error => PoolError::refused(RunRefusal::Start {
                        detail: Detail::new(error),
                    }),
                })?
            }
            StartFrom::Checkpoint(_) => {
                let root = checkpoint
                    .as_ref()
                    .ok_or_else(|| undecodable(RunInput::Checkpoint, &"the root was not read"))?;
                KernelMachine::open(program, bounds, root, source).map_err(|error| match error {
                    OpenError::Heap(fault) => heap_fault(fault),
                    error @ OpenError::Accounting { .. } => machine_breach(error),
                    error => PoolError::refused(RunRefusal::Restore {
                        refusal: restore_refusal(error),
                    }),
                })?
            }
        };
        machine.set_options(lash_kernel_vm::MachineOptions {
            observation: if start.observe_counts {
                lash_kernel_vm::ObservationMode::Counts
            } else {
                lash_kernel_vm::ObservationMode::Off
            },
            trace_path: path.is_some(),
            captions,
            function_origins,
            cell_save: save.map(|save| Box::new(save) as Box<dyn lash_kernel_vm::CellSave>),
            memory_limiter: match start.memory_limiter {
                true => Some(Box::new(WireLimiter {
                    wire: self.wire()?,
                    bound: lash_vm_protocol::LimitedBound::Memory,
                })),
                false => None,
            },
            charge_limiter: match start.charge_limiter {
                true => Some(Box::new(WireLimiter {
                    wire: self.wire()?,
                    bound: lash_vm_protocol::LimitedBound::Charge,
                })),
                false => None,
            },
        });
        Ok(Hosted {
            machine,
            memory: start.bounds.memory,
            ended: false,
            sealed: None,
            port,
            observe_sites: start.observe_sites,
            path,
        })
    }
    /// Decode and authenticate ordinary content before helper compatibility.
    /// This loads no executable helper bodies.
    fn content(&mut self, start: &Start) -> Result<WorkerContent, PoolError> {
        let link = start.link;
        let encoded_instances = &start.instances;
        let supplied_documents = &start.documents;
        let wire = self.wire()?;
        let resolve = |supplied: &lash_vm_protocol::message::StoredDocument| {
            let limits = self.bootstrap.tuning.kernel_data;
            let loaded = match supplied {
                lash_vm_protocol::message::StoredDocument::Cached { document } => {
                    if let Some(known) = self.embedding.documents.get(*document, limits)? {
                        return Ok(known);
                    }
                    self.embedding
                        .library_image()
                        .resolve_document(&wire.document(*document)?, limits)?
                }
                supplied => self
                    .embedding
                    .library_image()
                    .resolve_document(supplied, limits)?,
            };
            self.embedding.documents.keep(loaded.clone(), Some(limits));
            Ok::<_, PoolError>(loaded)
        };
        let document = resolve(&start.document)?;
        let instances: Vec<lash_kernel_vm::interface::PreparedInstance> =
            lash_vm_client::wire::decode(
                lash_protocol_types::engine_fault::PayloadKind::Start,
                encoded_instances,
            )?;
        let mut indexed = std::collections::BTreeMap::new();
        for instance in instances {
            if indexed.insert(instance.link, instance).is_some() {
                return Err(PoolError::payload(
                    lash_protocol_types::engine_fault::PayloadKind::Start,
                    "duplicate linked instance",
                ));
            }
        }
        let mut documents = std::collections::BTreeMap::new();
        for supplied in supplied_documents {
            let loaded = resolve(supplied)?;
            if documents
                .insert(supplied.identity(), Arc::clone(loaded.shared()))
                .is_some()
            {
                return Err(PoolError::payload(
                    lash_protocol_types::engine_fault::PayloadKind::Start,
                    "document identity or duplicate",
                ));
            }
        }
        documents
            .entry(document.id())
            .or_insert_with(|| Arc::clone(document.shared()));
        if indexed
            .values()
            .any(|instance| !documents.contains_key(&instance.document))
        {
            return Err(PoolError::payload(
                PayloadKind::Start,
                "linked instance document is absent",
            ));
        }
        let root = indexed.get(&link).ok_or_else(|| {
            PoolError::payload(
                lash_protocol_types::engine_fault::PayloadKind::Start,
                "root Link is not loaded",
            )
        })?;
        if root.document != document.id() || !documents.contains_key(&document.id()) {
            return Err(PoolError::payload(
                lash_protocol_types::engine_fault::PayloadKind::Start,
                "root Link names another document",
            ));
        }
        for document in documents.values() {
            lash_kernel_doc::validate_document_structure(document).map_err(|error| {
                PoolError::refused(RunRefusal::Start {
                    detail: Detail::new(error),
                })
            })?;
        }
        Ok(WorkerContent {
            document,
            indexed,
            documents,
        })
    }

    /// The content key is the token body itself rather than a claimed
    /// identity: a document reaches one closure on this worker, whose
    /// stored definitions the parent supplies alike each time. Holding the
    /// same Arc also reuses PreparedLibrary's admission and code; eviction
    /// lets its weak document entries be reclaimed on admission. A miss
    /// decodes the body once, under the worker's data limits, and takes
    /// the library the document runs on ([`crate::embedding::library_for`]): the
    /// frame's when it holds every function the document reaches, and
    /// otherwise one assembled with `definitions`, the stored definitions
    /// of the others (FIG-6985). A `fresh` start whose library is refused
    /// is refused as a start is, and a resume as a restore, naming the
    /// function.
    fn program(
        &mut self,
        start: &lash_vm_protocol::message::Start,
        fresh: bool,
        content: WorkerContent,
        selection: lash_kernel_doc::Selection,
    ) -> Result<Program, PoolError> {
        let WorkerContent {
            document,
            indexed,
            documents,
        } = content;
        let definitions = &start.definitions;
        let functions = &start.functions;
        let link = start.link;
        let supplied: Vec<(lash_kernel_value::names::FunctionId, &[u8])> = definitions
            .iter()
            .map(|definition| {
                (
                    lash_kernel_value::names::FunctionId::from_bytes(definition.function),
                    definition.body.0.as_slice(),
                )
            })
            .collect();
        let (library, closure) = crate::embedding::linked_library_for(
            self.embedding,
            document.shared(),
            &documents,
            &[(link, functions.clone())].into(),
            &supplied,
            self.bootstrap.tuning.kernel_data,
            &selection,
        )
        .map_err(|error| {
            PoolError::breach(ProtocolBreach::Machine {
                detail: Detail::new(error),
            })
        })?
        .map_err(|refusal| match fresh {
            true => PoolError::refused(RunRefusal::Start {
                detail: Detail::new(refusal),
            }),
            false => PoolError::refused(RunRefusal::Restore {
                refusal: refusal.into(),
            }),
        })?;
        // Admitted by identity: one validated before, in lowering or an
        // earlier decode, is not validated again (FIG-6931). A refusal is
        // the start's, which admits it again.
        library.admit_identified(
            document.shared(),
            document.id(),
            self.bootstrap.tuning.kernel_data,
            &selection,
        );
        let executable = match &selection {
            lash_kernel_doc::Selection::Fresh => {
                let mut executable = functions.clone();
                for document in documents.values() {
                    executable.extend(
                        self.embedding
                            .library_image()
                            .helpers()
                            .reach(&document.library)
                            .map_err(|error| {
                                PoolError::refused(RunRefusal::Start {
                                    detail: Detail::new(error),
                                })
                            })?,
                    );
                }
                executable
            }
            lash_kernel_doc::Selection::Restored(restored) => restored
                .live
                .iter()
                .map(|callable| callable.function)
                .chain(restored.pins.iter().copied())
                .collect(),
        };
        let program = Program {
            selection,
            functions: executable,
            closure: PageId(closure.reference().0),
            document: Arc::clone(document.shared()),
            library: library.clone(),
            link,
            instances: indexed,
            documents,
        };
        Ok(program)
    }

    /// Runs the machine one slice and says where the run stands. What it
    /// printed leaves first, in frames of its own.
    fn slice(&mut self, slice: u64, cancel: bool) -> Result<WorkerMessage, PoolError> {
        let wire = self.wire()?;
        let hosted = self.running()?;
        let mut host = WireHost {
            wire,
            cancel,
            printed: Vec::new(),
            ran: lash_kernel_vm::SiteCounts::new(),
            path: lash_kernel_vm::ExecutionPath::default(),
            caught: Vec::new(),
            sites: hosted
                .observe_sites
                .then(crate::host::EnteredSites::default),
        };
        #[cfg(feature = "cell-memory")]
        crate::cell_memory::attach(&mut hosted.machine);
        hosted.sealed = None;
        let port = hosted.port.clone();
        let step = hosted
            .machine
            .run(&mut host, slice)
            .map_err(|error| match error {
                lash_kernel_vm::MachineError::Heap(fault) => heap_fault(fault),
                other => machine_breach(other),
            })?;
        if hosted.observe_sites || !matches!(step, Step::Slice) {
            host.ran.extend(hosted.machine.take_counts());
        }
        if !matches!(step, Step::Slice) {
            host.caught = lash_kernel_vm::Machine::take_caught(&mut hosted.machine);
        }
        if hosted.path.is_some() {
            host.path = lash_kernel_vm::Machine::take_path(&mut hosted.machine)
                .map_err(|fault| machine_breach(format!("the path lost a task: {fault}")))?;
        }
        let meters = hosted.machine.meters();
        #[cfg(feature = "cell-memory")]
        if matches!(step, Step::Ended(_)) {
            crate::cell_memory::worker_finish(&hosted.machine).map_err(PoolError::io)?;
        }
        let memory = hosted.memory;
        if matches!(step, Step::Ended(_)) {
            hosted.ended = true;
        }
        let meters = RunMeters {
            charged: meters.charged,
            memory: meters.memory,
            live_tasks: meters.live_tasks,
        };
        // A park that requests something is saved as it stands, so it is
        // sealed now, where the parent would ask for it next. What it
        // sealed crosses with the park, and a finished cell's carry with
        // the run's end.
        let candidate = match &step {
            Step::Parked(park) if requests_something(park) => Some(self.seal()?),
            _ => None,
        };
        let seal = match &step {
            Step::Parked(_) | Step::Ended(_) => port.take_batch(),
            Step::Slice => None,
        };
        self.progress(WorkerPhase::Serializing)?;
        let Some(chunks) = self.printed_chunks(&host.printed, memory)? else {
            return Ok(WorkerMessage::LimitExceeded {
                limit: WorkerLimit::Observations,
            });
        };
        for payload in chunks {
            self.send(WorkerMessage::Printed { payload })?;
        }
        let sites = host.take_sites()?;
        let message = match step {
            Step::Parked(park) => WorkerMessage::Parked {
                park: self.encode(PayloadKind::Park, &ParkWire::from(park))?,
                meters,
                seal,
                candidate,
                sites,
            },
            Step::Slice => WorkerMessage::Slice { meters, sites },
            Step::Ended(end) => {
                // A finished cell's carry crosses to the parent's staging
                // with the run's end: each of its pages is named by the
                // capability its seal or read granted, or as a page of the
                // batch that crosses with it.
                let end = EndWire::of(end, |page| port.link(page));
                WorkerMessage::Ended {
                    end: self.encode(PayloadKind::End, &end)?,
                    meters,
                    seal,
                    sites,
                }
            }
        };
        Ok(message)
    }
    fn encode<T: serde::Serialize>(
        &self,
        kind: PayloadKind,
        value: &T,
    ) -> Result<EncodedPayload, PoolError> {
        let payload = wire::encode(kind, value)?;
        // A completed run carries every session binding along with its
        // result. Its envelope is state, rather than one effect value.
        let bound = if kind == PayloadKind::End {
            self.bootstrap.state
        } else {
            self.bootstrap.effect
        };
        if payload.0.len() as u64 > bound {
            let size = payload.0.len() as u64;
            let limit = if kind == PayloadKind::End {
                WorkerLimit::VmState { size, bound }
            } else {
                WorkerLimit::EffectValue { size, bound }
            };
            return Err(InfrastructureOutcome::WorkerLimitExceeded { limit }.into());
        }
        Ok(payload)
    }
    /// A slice's printed values as the payloads of the frames that carry
    /// them, each within the frame's bounds (FIG-4458); `None` when they
    /// outgrow the run's heap bound, or one alone outgrows a frame.
    fn printed_chunks(
        &self,
        printed: &[Datum],
        budget: u64,
    ) -> Result<Option<Vec<EncodedPayload>>, PoolError> {
        if printed.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let mut chunker = self.codec.observation_chunker()?;
        for value in printed {
            let encoded = wire::encode(PayloadKind::Printed, value)?;
            match chunker.push(&encoded.0) {
                Ok(()) => {}
                Err(
                    CodecRefusal::FrameTooLarge { .. }
                    | CodecRefusal::NodeLimitExceeded { .. }
                    | CodecRefusal::DepthExceeded { .. }
                    | CodecRefusal::AllocationExceeded { .. },
                ) => return Ok(None),
                Err(
                    refusal @ (CodecRefusal::Truncated { .. }
                    | CodecRefusal::BadMagic
                    | CodecRefusal::Malformed { .. }
                    | CodecRefusal::TrailingBytes { .. }
                    | CodecRefusal::PageBatch { .. }),
                ) => return Err(refusal.into()),
            }
            if chunker.bytes() > budget {
                return Ok(None);
            }
        }
        Ok(Some(chunker.finish()))
    }
    /// Sends the step's answer, and hands its message back.
    fn respond(&mut self, message: WorkerMessage) -> Result<WorkerMessage, PoolError> {
        // Encode while the serialization deadline is active. Reserve the
        // header after Responding without advancing the real fence yet.
        let mut fence = self
            .fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .outgoing;
        fence.next_header();
        let header = fence.next_header();
        let (message, bytes) = match encode_worker(&self.codec, header, message) {
            (
                message,
                Err(PoolError::Infrastructure(InfrastructureOutcome::WorkerLimitExceeded {
                    limit,
                })),
            ) => (
                message,
                encode_worker(&self.codec, header, WorkerMessage::LimitExceeded { limit }).1?,
            ),
            (message, bytes) => (message, bytes?),
        };
        crate::perf_witness::buffer(
            bytes.capacity()
                + self
                    .inbound
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .buffer_capacity(),
        );
        // Responding and the answer leave in one write: nothing happens
        // between them, and the parent then wakes once for both (FIG-4433).
        let responding = {
            let mut fences = self
                .fences
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let responding = fences.encode(
                &self.codec,
                WorkerMessage::Progress {
                    phase: WorkerPhase::Responding,
                    cpu_nanos: cpu_nanos()?,
                    meters: self.run_meters(),
                    work: crate::perf_witness::take().map(Box::new),
                },
            )?;
            fences.outgoing.next_header();
            responding
        };
        crate::perf_witness::buffer(
            bytes.capacity()
                + responding.capacity()
                + self
                    .inbound
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .buffer_capacity(),
        );
        write_frames(
            &mut self.pipe,
            responding,
            &bytes,
            self.bootstrap.serialization,
        )?;
        Ok(message)
    }
}

/// Whether a park requests an effect or a sleep it has not withdrawn: a
/// park the parent saves.
fn requests_something(park: &lash_kernel_vm::Park) -> bool {
    park.requests.iter().any(|request| {
        let wait = match request {
            lash_kernel_vm::Request::Effect(effect) => effect.wait,
        };
        !park.withdrawn.contains(&wait)
    })
}

pub(crate) fn cpu_nanos() -> Result<u64, PoolError> {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::zeroed();
    // SAFETY: time points to valid writable timespec storage.
    #[expect(
        unsafe_code,
        reason = "process CPU clock excludes IPC and parent waits"
    )]
    let result = unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, time.as_mut_ptr()) };
    if result != 0 {
        return Err(PoolError::io(std::io::Error::last_os_error()));
    }
    #[expect(unsafe_code, reason = "successful clock_gettime initialized timespec")]
    let time = unsafe { time.assume_init() };
    Ok((time.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(time.tv_nsec as u64))
}

/// A heap fault the worker met itself, as the protocol carries it: a host
/// outcome of the activation, never the guest's (FIG-6422).
fn heap_fault(fault: lash_kernel_doc::heap::HeapFault) -> PoolError {
    use lash_kernel_doc::heap::{HeapFault as Kernel, HostResource as KernelResource};
    let detail = || Detail::new(&fault);
    let fault = match &fault {
        Kernel::Missing { .. } => {
            lash_protocol_types::engine_fault::HeapFault::Missing { detail: detail() }
        }
        Kernel::Corrupt { .. } => {
            lash_protocol_types::engine_fault::HeapFault::Corrupt { detail: detail() }
        }
        Kernel::Unauthorized { .. } => {
            lash_protocol_types::engine_fault::HeapFault::Unauthorized { detail: detail() }
        }
        Kernel::LeaseLost => lash_protocol_types::engine_fault::HeapFault::LeaseLost,
        Kernel::Exhausted { resource } => lash_protocol_types::engine_fault::HeapFault::Exhausted {
            resource: match resource {
                KernelResource::TraceBudget => {
                    lash_protocol_types::engine_fault::HostResource::TraceBudget
                }
                KernelResource::CleanResidency => {
                    lash_protocol_types::engine_fault::HostResource::CleanResidency
                }
                KernelResource::DirtyResidency => {
                    lash_protocol_types::engine_fault::HostResource::DirtyResidency
                }
                KernelResource::PinnedPages => {
                    lash_protocol_types::engine_fault::HostResource::PinnedPages
                }
                KernelResource::StagingBytes => {
                    lash_protocol_types::engine_fault::HostResource::StagingBytes
                }
                KernelResource::ReadBatch => {
                    lash_protocol_types::engine_fault::HostResource::ReadBatch
                }
                KernelResource::HostWork => {
                    lash_protocol_types::engine_fault::HostResource::HostWork
                }
            },
        },
        Kernel::Transport(_) => {
            lash_protocol_types::engine_fault::HeapFault::Transport { detail: detail() }
        }
        Kernel::Malformed(_) => {
            lash_protocol_types::engine_fault::HeapFault::Malformed { detail: detail() }
        }
        _ => lash_protocol_types::engine_fault::HeapFault::Malformed { detail: detail() },
    };
    InfrastructureOutcome::HeapFault { fault }.into()
}

fn helper_compatibility_refusal(
    error: lash_kernel_state::paged::HelperCompatibilityError,
) -> PoolError {
    match error {
        lash_kernel_state::paged::HelperCompatibilityError::Heap(fault) => heap_fault(fault),
        lash_kernel_state::paged::HelperCompatibilityError::HelperUnavailable {
            reason,
            function,
        } => PoolError::refused(RunRefusal::Restore {
            refusal: KernelRestoreRefusal::HelperUnavailable { reason, function },
        }),
    }
}

/// The kernel's refusal of a root as the protocol carries it.
fn restore_refusal(error: OpenError) -> KernelRestoreRefusal {
    match error {
        OpenError::HelperUnavailable { reason, function } => {
            KernelRestoreRefusal::HelperUnavailable { reason, function }
        }
        OpenError::MissingFunction { function } => KernelRestoreRefusal::DefinitionRefused {
            function: function.to_string(),
            problem: "the worker's library lacks it".to_owned(),
        },
        OpenError::NotAdmitted(invalid) => KernelRestoreRefusal::NotAdmitted {
            reason: invalid.to_string(),
        },
        OpenError::DocumentMismatch { parked, given } => KernelRestoreRefusal::DocumentMismatch {
            parked: parked.to_string(),
            given: given.to_string(),
        },
        OpenError::KernelVersion { parked, supported } => {
            KernelRestoreRefusal::KernelVersion { parked, supported }
        }
        other => KernelRestoreRefusal::Malformed {
            problem: other.to_string(),
        },
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn machine_breach(error: impl std::fmt::Display) -> PoolError {
    PoolError::breach(ProtocolBreach::Machine {
        detail: Detail::new(error),
    })
}

#[cfg(test)]
mod helper_restore_tests;
#[cfg(test)]
mod tests;
