use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use crate::PoolError;
use crate::embedding::Embedding;
use crate::host::{Wire, WireHost};
use lash_core_store::checkpoint_graph::bodies::OwnerBinding;
use lash_core_store::checkpoint_graph::{DecodeLimits, GraphRefusal, NodeKind};
use lash_kernel_doc::{Datum, Document};
use lash_kernel_vm::{Bounds, KernelMachine, Machine, Program, Step};
use lash_vm_client::graph::{self, GraphStateFailure};
#[cfg(test)]
use lash_vm_client::ipc::read_frame;
use lash_vm_client::ipc::{Bootstrap, FrameSource, write_frame, write_frames};
use lash_vm_client::wire::{self, EndWire, OutcomeWire, ParkWire, StartWire};
use lash_vm_protocol::*;

/// The one run a worker hosts between resets.
struct Hosted {
    machine: KernelMachine,
    /// The identity of the document the run executes, as its parked state
    /// names it.
    document: String,
    /// The kernel version the document states, which its parked state is
    /// sealed under.
    kernel: u32,
    /// The run's heap bound, which also bounds what one slice may print
    /// (FIG-4458).
    memory: u64,
    ended: bool,
    /// The release manifest the run pins, once it resumed from graph state;
    /// a run started here pins the release the build writes.
    release: Option<lash_kernel_doc::graph_tokens::ChildRef>,
    /// The execution the run's state is bound to at each park that requests
    /// something, as its start named it.
    export: Option<EncodedPayload>,
    /// The export of the run's last park that its `Parked` did not carry:
    /// one that failed, or that did not fit one frame with the park. The
    /// parent's `ExportGraph` takes it, failure included, as if the export
    /// ran then.
    staged: Option<Result<SealedGraphState, PoolError>>,
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
    (frame.message, bytes)
}

impl<'embedding> Server<'embedding> {
    pub(crate) fn new(
        pipe: UnixStream,
        codec: FrameCodec,
        bootstrap: Bootstrap,
        embedding: &'embedding Embedding,
    ) -> Result<Self, PoolError> {
        // A ready worker can publish and restore checkpoints without first
        // staging its immutable helper catalog during a guest turn, and runs
        // its first document over a library compiled here, before the
        // parent hands it one: its first turn costs what a warm one does
        // (FIG-5922).
        embedding
            .release_manifests()
            .map_err(|error| machine_breach(error.to_string()))?;
        embedding
            .prepared_for(lash_kernel_doc::KERNEL_VERSION)
            .map_err(|error| machine_breach(error.to_string()))?;
        let tuning = bootstrap.tuning;
        let mut server = Self {
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
            })),
            owner: None,
            prepared: false,
            cpu_ceiling: None,
        };
        server.send(WorkerMessage::Ready {
            protocol_version: WORKER_PROTOCOL_VERSION,
            crate_version: env!("CARGO_PKG_VERSION").to_owned(),
        })?;
        Ok(server)
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
        match WorkerRefusal::testimony(error.clone().into_outcome()) {
            Some(message) => self.send(message),
            None => Ok(()),
        }
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
                } => {
                    if let Some(start) = &start {
                        self.admit_start(start)?;
                    }
                    self.progress(WorkerPhase::Computing)?;
                    if let Some(start) = start {
                        self.hosted = Some(self.start(*start)?);
                    }
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
                ParentMessage::ExportGraph { binding } => {
                    self.progress(WorkerPhase::Computing)?;
                    let exported = match self.staged_export(&binding) {
                        Some(staged) => staged,
                        None => self.export_graph(&binding),
                    };
                    let message = match exported {
                        Err(PoolError::Infrastructure(
                            InfrastructureOutcome::WorkerLimitExceeded { limit },
                        )) => WorkerMessage::LimitExceeded { limit },
                        result => WorkerMessage::ExportedGraph { state: result? },
                    };
                    self.progress(WorkerPhase::Serializing)?;
                    self.respond(message)?;
                }
                ParentMessage::HostAnswer { .. } => {
                    // A read is answered inside the slice that made it.
                    return Err(PoolError::breach(SequenceFault::NoPendingRead));
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
        hosted.staged = None;
        hosted
            .machine
            .deliver(lash_kernel_vm::WaitId(delivery.wait), outcome.into())
            .map_err(machine_breach)?;
        Ok(())
    }
    /// The export the run's last park made under `binding`, if it made one
    /// its `Parked` did not carry.
    fn staged_export(
        &mut self,
        binding: &EncodedPayload,
    ) -> Option<Result<SealedGraphState, PoolError>> {
        let hosted = self.hosted.as_mut()?;
        if hosted.export.as_ref() != Some(binding) {
            return None;
        }
        hosted.staged.take()
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
    /// A machine for the run: one that has executed nothing, or one rebuilt
    /// from the parked state. The document is validated and compiled against
    /// the registry this worker assembled when it started, with each function
    /// redeclared for the document's kernel version when that is a
    /// successor's; nothing is loaded from the document.
    fn start(&mut self, start: Start) -> Result<Hosted, PoolError> {
        self.wire = None;
        self.codec.check_payload(&start.document.0)?;
        let undecodable = |input: RunInput, error: &dyn std::fmt::Display| {
            PoolError::refused(RunRefusal::Undecodable {
                input,
                detail: Detail::new(error),
            })
        };
        let document = wire::unwrap(PayloadKind::Document, &start.document)?;
        let document = std::str::from_utf8(&document)
            .map_err(|error| undecodable(RunInput::Document, &error))
            .and_then(|text| {
                Document::from_json(text).map_err(|error| undecodable(RunInput::Document, &error))
            })?;
        let identity = document
            .identity()
            .map_err(|error| undecodable(RunInput::Document, &error))?
            .to_string();
        let kernel = document.manifest.kernel;
        let library = self.embedding.prepared_for(kernel).map_err(|error| {
            PoolError::breach(ProtocolBreach::Machine {
                detail: Detail::new(error),
            })
        })?;
        let program = Program {
            document: Arc::new(document),
            library: library.clone(),
        };
        let bounds = Bounds {
            charge: start.bounds.charge,
            memory: start.bounds.memory,
            call_depth: start.bounds.call_depth,
            live_tasks: start.bounds.live_tasks,
            requests_per_park: start.bounds.requests_per_park,
            join_members: start.bounds.join_members,
        };
        let mut release = None;
        let machine = match start.from {
            StartFrom::Fresh(payload) => {
                self.codec.check_payload(&payload.0)?;
                let from: StartWire = wire::decode(PayloadKind::Start, &payload)
                    .map_err(|error| undecodable(RunInput::Start, &error))?;
                KernelMachine::start(program, bounds, from.into()).map_err(|error| {
                    PoolError::refused(RunRefusal::Start {
                        detail: Detail::new(error),
                    })
                })?
            }
            StartFrom::ParkedGraph(parked) => {
                parked
                    .state
                    .check(&GraphStateExpectation {
                        owner: &start.owner,
                        kernel: lash_vm_client::kernel_reads(),
                        document: Some(&identity),
                        root_kinds: &[NodeKind::KERNEL_WHOLE.0],
                        max_nodes: self.bootstrap.state,
                        max_bytes: self.bootstrap.state,
                    })
                    .map_err(InfrastructureOutcome::input_graph_state)?;
                let manifests = self.embedding.release_manifests().map_err(|error| {
                    PoolError::breach(ProtocolBreach::Machine {
                        detail: Detail::new(error),
                    })
                })?;
                let registry = Arc::clone(program.library.registry());
                // A large value shared by many references is one leaf but
                // reads back whole at each: the logical body is bounded by
                // the state and the run's heap together.
                let limits = DecodeLimits {
                    logical_bytes: self.bootstrap.state.saturating_add(bounds.memory),
                    ..DecodeLimits::within_total(self.bootstrap.state)
                };
                let (machine, pinned) =
                    graph::import_parked(program, bounds, &parked, manifests, &registry, limits)
                        .map_err(|failure| match failure {
                            GraphStateFailure::Restore(refusal) => {
                                PoolError::refused(RunRefusal::Restore { refusal })
                            }
                            other => undecodable(RunInput::ParkedRun, &other),
                        })?;
                release = Some(pinned);
                machine
            }
        };
        Ok(Hosted {
            machine,
            document: identity,
            kernel,
            memory: start.bounds.memory,
            ended: false,
            release,
            export: start.export,
            staged: None,
        })
    }
    /// Runs the machine one slice and says where the run stands. What it
    /// printed leaves first, in frames of its own.
    fn slice(&mut self, slice: u64, cancel: bool) -> Result<WorkerMessage, PoolError> {
        let mut host = WireHost {
            wire: self.wire()?,
            cancel,
            printed: Vec::new(),
        };
        let hosted = self.running()?;
        hosted.staged = None;
        let step = hosted
            .machine
            .run(&mut host, slice)
            .map_err(machine_breach)?;
        let meters = hosted.machine.meters();
        let memory = hosted.memory;
        if matches!(step, Step::Ended(_)) {
            hosted.ended = true;
        }
        let meters = RunMeters {
            charged: meters.charged,
            memory: meters.memory,
            live_tasks: meters.live_tasks,
        };
        // A park that requests something is saved as it stands, so its
        // state is exported now, where the parent would ask for it next.
        let export = match &step {
            Step::Parked(park) if requests_something(park) => hosted.export.clone(),
            _ => None,
        };
        let state = match export.map(|binding| self.export_graph(&binding)) {
            Some(Ok(state)) => Some(state),
            Some(Err(failure)) => {
                self.running()?.staged = Some(Err(failure));
                None
            }
            None => None,
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
        let message = match step {
            Step::Parked(park) => WorkerMessage::Parked {
                park: self.encode(PayloadKind::Park, &ParkWire::from(park))?,
                meters,
                state,
            },
            Step::Slice => WorkerMessage::Slice { meters },
            Step::Ended(end) => {
                let end: EndWire = self.end_state(&end)?;
                WorkerMessage::Ended {
                    end: self.encode(PayloadKind::End, &end)?,
                    meters,
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
                    | CodecRefusal::TrailingBytes { .. }),
                ) => return Err(refusal.into()),
            }
            if chunker.bytes() > budget {
                return Ok(None);
            }
        }
        Ok(Some(chunker.finish()))
    }
    /// The run's end as TerminalWhole graph state, sealed under its owner,
    /// the kernel version and its document; none for a cancelled run.
    fn end_state(&self, end: &lash_kernel_vm::End) -> Result<EndWire, PoolError> {
        let owner = self
            .owner
            .clone()
            .ok_or_else(|| PoolError::breach(SequenceFault::MissingOwner))?;
        let bound = self.bootstrap.state;
        let hosted = self
            .hosted
            .as_ref()
            .ok_or_else(|| PoolError::breach(SequenceFault::NoRun))?;
        graph::export_end(end, owner, hosted.kernel, &hosted.document, bound)
            .map_err(|failure| state_failure(failure, bound))
    }
    /// The run's state at this park as checkpoint graph state, bound to the
    /// execution `binding` names and pinning the run's release.
    fn export_graph(&mut self, binding: &EncodedPayload) -> Result<SealedGraphState, PoolError> {
        let owner = self
            .owner
            .clone()
            .ok_or_else(|| PoolError::breach(SequenceFault::MissingOwner))?;
        let binding: OwnerBinding = lash_kernel_doc::graph_tokens::decode_body(&binding.0)
            .map_err(|error| PoolError::payload(PayloadKind::ParkedRun, error))?;
        let bound = self.bootstrap.state;
        let current = self
            .embedding
            .release_manifests()
            .map_err(machine_breach)?
            .manifests
            .current()
            .ok_or_else(|| machine_breach("the build holds no helper release"))?;
        let hosted = self.running()?;
        let release = hosted.release.unwrap_or(current);
        graph::export_parked(
            &mut hosted.machine,
            owner,
            &binding,
            release,
            &hosted.document,
            bound,
        )
        .map_err(|failure| state_failure(failure, bound))
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
            // A state that does not fit one frame with its park waits for
            // the parent's `ExportGraph`, which then carries it alone.
            (
                WorkerMessage::Parked {
                    park,
                    meters,
                    state: Some(state),
                },
                bytes,
            ) if !bytes
                .as_ref()
                .is_ok_and(|bytes| self.codec.check_frame(bytes).is_ok()) =>
            {
                self.running()?.staged = Some(Ok(state));
                let parked = WorkerMessage::Parked {
                    park,
                    meters,
                    state: None,
                };
                match encode_worker(&self.codec, header, parked) {
                    (
                        message,
                        Err(PoolError::Infrastructure(
                            InfrastructureOutcome::WorkerLimitExceeded { limit },
                        )),
                    ) => (
                        message,
                        encode_worker(&self.codec, header, WorkerMessage::LimitExceeded { limit })
                            .1?,
                    ),
                    (message, bytes) => (message, bytes?),
                }
            }
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
                },
            )?;
            fences.outgoing.next_header();
            responding
        };
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
            lash_kernel_vm::Request::Sleep(sleep) => sleep.wait,
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

/// A state that did not stage: over the state bound, the run's limit, and
/// otherwise the machine's breach.
fn state_failure(failure: GraphStateFailure, bound: u64) -> PoolError {
    match failure {
        GraphStateFailure::Graph(GraphRefusal::EncodeMemoryLimit) => {
            InfrastructureOutcome::WorkerLimitExceeded {
                // The staging stopped at the bound: the state is larger
                // than it.
                limit: WorkerLimit::VmState {
                    size: bound.saturating_add(1),
                    bound,
                },
            }
            .into()
        }
        other => machine_breach(other),
    }
}

fn machine_breach(error: impl std::fmt::Display) -> PoolError {
    PoolError::breach(ProtocolBreach::Machine {
        detail: Detail::new(error),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-5497: the worker must obey the parent's response IO deadline.
    #[test]
    fn parent_serialization_deadline_bounds_worker_response_write() {
        let mut config =
            lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"));
        config.deadlines.serialization = Duration::from_millis(5);
        let codec = FrameCodec::new(config.protocol.decode);
        let (pipe, mut parent) = UnixStream::pair().expect("pipe");
        let embedding = crate::embedding::Embedder::kernel()
            .and_then(crate::embedding::Embedder::finish)
            .expect("embedding");
        let mut server =
            Server::new(pipe, codec.clone(), Bootstrap::from(&config), &embedding).expect("server");
        read_frame(&mut parent, &codec, Instant::now() + Duration::from_secs(1)).expect("ready");
        let drain = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            // Releasing backpressure lets the unfixed write finish too, so
            // this regression fails promptly instead of waiting thirty seconds.
            let _ = read_frame(&mut parent, &codec, Instant::now() + Duration::from_secs(1));
        });
        let result = server.send(WorkerMessage::Prepared {
            response: EncodedPayload(vec![0; 1024 * 1024]),
        });
        drop(server);
        drain.join().expect("drain");
        assert!(
            result.is_err(),
            "the host's five-millisecond IO deadline must refuse the write"
        );
    }

    #[test]
    fn an_oversized_non_observation_frame_preserves_its_fence_and_typed_cause() {
        let mut config =
            lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"));
        config.protocol.decode.max_frame_bytes = 1024;
        let codec = FrameCodec::new(config.protocol.decode);
        let (pipe, mut parent) = UnixStream::pair().expect("pipe");
        let embedding = crate::embedding::Embedder::kernel()
            .and_then(crate::embedding::Embedder::finish)
            .expect("embedding");
        let mut server =
            Server::new(pipe, codec.clone(), Bootstrap::from(&config), &embedding).expect("server");
        let mut fence = MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0));
        let ready = read_frame(&mut parent, &codec, Instant::now() + Duration::from_secs(1))
            .expect("ready");
        fence
            .admit(&codec.decode_worker(&ready).expect("ready frame").header)
            .expect("ready fence");
        let message = WorkerMessage::Prepared {
            response: EncodedPayload(vec![0; 2048]),
        };
        let size = rmp_serde::to_vec_named(&WorkerFrame {
            header: fence.next_header_copy(),
            message: message.clone(),
        })
        .expect("measure")
        .len() as u64
            + FRAME_HEADER_BYTES as u64;
        let error = server.send(message).expect_err("the frame is too large");
        server.refuse(&error).expect("typed refusal frame");
        let bytes = read_frame(&mut parent, &codec, Instant::now() + Duration::from_secs(1))
            .expect("next frame");
        let refused = codec.decode_worker(&bytes).expect("frame");
        fence
            .admit(&refused.header)
            .expect("an encode refusal must not consume a transport sequence");
        let PoolError::Infrastructure(outcome) = error else {
            panic!("typed cause: {error:?}")
        };
        assert_eq!(
            serde_json::to_value(&outcome).expect("cause"),
            serde_json::json!({
                "worker_limit_exceeded": { "limit": { "frame": {
                    "kind": "prepared", "size": size, "bound": 1024
                } } }
            })
        );
        let WorkerMessage::LimitExceeded { limit } = refused.message else {
            panic!("typed refusal")
        };
        assert_eq!(
            outcome,
            InfrastructureOutcome::WorkerLimitExceeded { limit }
        );
        assert!(!outcome.is_retryable());
    }
}
