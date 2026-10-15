use std::collections::VecDeque;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use crate::PoolError;
use crate::embedding::Embedding;
use crate::host::{PagePort, Wire, WireHost, WirePageSource, grant_of, link_grant};
use lash_kernel_doc::graph_tokens::{ChildRef, decode_body};
use lash_kernel_doc::heap::{LogicalBytes, PageId, PageRef};
use lash_kernel_doc::{Datum, Document};
use lash_kernel_state::AdmittedWait;
use lash_kernel_state::paged::census_impl::Census;
use lash_kernel_state::paged::{
    AccountingCertificate, AccountingMetric, AllocationCensus, CheckpointRoot, PageSource,
    SealBudget, heap_accounting,
};
use lash_kernel_vm::{Bounds, CellStart, KernelMachine, Machine, OpenError, Program, Step};
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
    path: Option<std::collections::BTreeMap<lash_kernel_doc::Site, u32>>,
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
    /// Immutable preparation survives reset, scoped to this embedding's
    /// fixed registry. Exact wire content includes format and helper ids.
    /// At most sixteen documents and eight MiB of encoded input are held;
    /// a larger document runs normally without becoming resident.
    documents: VecDeque<(EncodedPayload, Program, String)>,
    fences: Arc<Mutex<Fences>>,
    owner: Option<VmOwner>,
    /// Whether the lease's one `Prepare` crossed, and no run started since.
    prepared: bool,
    cpu_ceiling: Option<libc::rlim_t>,
    #[cfg(test)]
    document_decodes: usize,
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
            documents: VecDeque::new(),
            fences: Arc::new(Mutex::new(Fences {
                incoming: None,
                outgoing: MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0)),
                next_read: 0,
                next_page: 0,
            })),
            owner: None,
            prepared: false,
            cpu_ceiling: None,
            #[cfg(test)]
            document_decodes: 0,
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
                ParentMessage::Pages { .. } => {
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
        hosted.sealed = None;
        hosted
            .machine
            .deliver(lash_kernel_vm::WaitId(delivery.wait), outcome.into())
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
        let root = loop {
            let sealed = hosted
                .machine
                .seal(SealBudget {
                    pages: u64::MAX,
                    bytes: u64::MAX,
                })
                .map_err(|error| match error {
                    lash_kernel_vm::SealError::Heap(fault) => heap_fault(fault),
                    other => machine_breach(other),
                })?;
            if let Some(sealed) = sealed {
                break sealed;
            }
        };
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
        // creation. A worker whose host did not name that dialect refuses
        // before it reads the document or the checkpoint (FIG-5990).
        if !self.embedding.installs(&start.dialect) {
            return Err(PoolError::refused(RunRefusal::DialectNotInstalled {
                dialect: start.dialect,
            }));
        }
        self.codec.check_payload(&start.document.0)?;
        let undecodable = |input: RunInput, error: &dyn std::fmt::Display| {
            PoolError::refused(RunRefusal::Undecodable {
                input,
                detail: Detail::new(error),
            })
        };
        let (mut program, identity) = self.program(&start.document)?;
        let path = start.observe_path.then(|| program.document.site_ordinals());
        let bounds = Bounds {
            charge: start.bounds.charge,
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
        let manifests = &self
            .embedding
            .release_manifests()
            .map_err(machine_breach)?
            .manifests;
        let save = start
            .save
            .as_ref()
            .map(|payload| {
                self.codec.check_payload(&payload.0)?;
                let save: wire::SaveWire = wire::decode(PayloadKind::Start, payload)
                    .map_err(|error| undecodable(RunInput::Start, &error))?;
                Ok::<_, PoolError>(crate::save::DialectSave {
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
        let mut machine = match start.from {
            StartFrom::Fresh(payload) => {
                self.codec.check_payload(&payload.0)?;
                let from: StartWire = wire::decode(PayloadKind::Start, &payload)
                    .map_err(|error| undecodable(RunInput::Start, &error))?;
                let release = manifests.current(&start.dialect).ok_or_else(|| {
                    machine_breach(format!(
                        "the build holds no helper release of the dialect `{}`",
                        start.dialect
                    ))
                })?;
                program.release = PageId(release.0);
                let session = from
                    .session
                    .as_ref()
                    .map(|carry| carry.session(link_grant))
                    .transpose()
                    .map_err(|error| undecodable(RunInput::Start, &error))?;
                let census = census(
                    session
                        .as_ref()
                        .map(|session| heap_accounting(&mut source, session.heap))
                        .transpose()
                        .map_err(heap_fault)?,
                    bounds.memory,
                )?;
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
                    Box::new(source),
                    census,
                )
                .map_err(|error| match error {
                    lash_kernel_vm::StartError::Heap(fault) => heap_fault(fault),
                    error => PoolError::refused(RunRefusal::Start {
                        detail: Detail::new(error),
                    }),
                })?
            }
            StartFrom::Checkpoint(checkpoint) => {
                // The root's bounded header first, through the page
                // exchange; then only what the run touches.
                let page = PageRef {
                    id: PageId(checkpoint.root.page),
                    grant: grant_of(&checkpoint.root),
                };
                let answers = source.read(&[page]).map_err(heap_fault)?;
                let answer = answers
                    .first()
                    .ok_or_else(|| undecodable(RunInput::Checkpoint, &"the root was not read"))?;
                let root = CheckpointRoot::of_answer(answer).map_err(|fault| {
                    PoolError::refused(RunRefusal::Checkpoint {
                        refusal: CheckpointRefusal::Malformed {
                            problem: fault.to_string(),
                        },
                    })
                })?;
                if root.owner != owner {
                    return Err(PoolError::refused(RunRefusal::Checkpoint {
                        refusal: CheckpointRefusal::WrongOwner {
                            expected: hex(&owner.0),
                            found: hex(&root.owner.0),
                        },
                    }));
                }
                if manifests.dialect_of(&ChildRef(root.release.0)).is_none() {
                    return Err(PoolError::refused(RunRefusal::Restore {
                        refusal: KernelRestoreRefusal::ReleaseMismatch {
                            pinned: hex(&root.release.0),
                        },
                    }));
                }
                program.release = root.release;
                let admissions = checkpoint
                    .admissions
                    .iter()
                    .map(|admission| decode_body::<AdmittedWait>(&admission.0))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| undecodable(RunInput::Checkpoint, &error))?;
                let census = census(
                    Some(heap_accounting(&mut source, root.heap).map_err(heap_fault)?),
                    bounds.memory,
                )?;
                KernelMachine::open(program, bounds, &root, admissions, Box::new(source), census)
                    .map_err(|error| match error {
                        OpenError::Heap(fault) => heap_fault(fault),
                        error => PoolError::refused(RunRefusal::Restore {
                            refusal: restore_refusal(error),
                        }),
                    })?
            }
        };
        machine.set_observe(if start.observe_counts {
            lash_kernel_vm::Observe::Counts
        } else {
            lash_kernel_vm::Observe::Off
        });
        let _ = identity;
        if let Some(save) = save {
            machine.set_cell_save(Box::new(save));
        }
        if path.is_some() {
            machine.trace_path();
        }
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
    /// The content key is verified bytes rather than a claimed identity.
    /// Holding the same Arc also reuses PreparedLibrary's admission and code;
    /// eviction lets its weak document entries be reclaimed on admission.
    fn program(&mut self, encoded: &EncodedPayload) -> Result<(Program, String), PoolError> {
        if let Some(index) = self
            .documents
            .iter()
            .position(|(bytes, _, _)| bytes == encoded)
            && let Some(hit) = self.documents.remove(index)
        {
            let result = (hit.1.clone(), hit.2.clone());
            self.documents.push_back(hit);
            return Ok(result);
        }
        let undecodable = |input: RunInput, error: &dyn std::fmt::Display| {
            PoolError::refused(RunRefusal::Undecodable {
                input,
                detail: Detail::new(error),
            })
        };
        #[cfg(test)]
        {
            self.document_decodes += 1;
        }
        let document = wire::unwrap(PayloadKind::Document, encoded)?;
        let document = std::str::from_utf8(&document)
            .map_err(|error| undecodable(RunInput::Document, &error))
            .and_then(|text| {
                Document::from_json_with_limits(text, self.bootstrap.tuning.kernel_data)
                    .map_err(|error| undecodable(RunInput::Document, &error))
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
        // A run's start names the release it pins.
        let program = Program {
            release: PageId([0; 32]),
            document: Arc::new(document),
            library: library.clone(),
        };
        const BYTES: usize = 8 * 1024 * 1024;
        if encoded.0.len() <= BYTES {
            while self.documents.len() >= 16
                || self
                    .documents
                    .iter()
                    .map(|(bytes, _, _)| bytes.0.len())
                    .sum::<usize>()
                    + encoded.0.len()
                    > BYTES
            {
                self.documents.pop_front();
            }
            self.documents
                .push_back((encoded.clone(), program.clone(), identity.clone()));
        }
        Ok((program, identity))
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
            ran: Vec::new(),
            path: Vec::new(),
            sites: hosted
                .observe_sites
                .then(crate::host::EnteredSites::default),
        };
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
            host.ran.extend(hosted.machine.take_ran());
        }
        if let Some(ordinals) = &hosted.path {
            host.path = hosted
                .machine
                .take_path()
                .map_err(|fault| machine_breach(format!("the path lost a task: {fault}")))?
                .into_iter()
                .map(|(task, sites)| {
                    let starts = sites
                        .iter()
                        .map(|site| {
                            ordinals.get(site).copied().ok_or_else(|| {
                                machine_breach(format!("the path names no document site {site}"))
                            })
                        })
                        .collect::<Result<Vec<u32>, PoolError>>()?;
                    Ok((task, starts))
                })
                .collect::<Result<_, PoolError>>()?;
        }
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
                    work: crate::perf_witness::take(),
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

/// A census over a heap whose persisted accounting is `accounting` (a
/// fresh heap's when none), held to the run's heap bound.
fn census(
    accounting: Option<AccountingCertificate>,
    memory: u64,
) -> Result<Box<dyn AllocationCensus>, PoolError> {
    let accounting = accounting.unwrap_or(AccountingCertificate {
        metric: AccountingMetric::LocalCostV1,
        upper: LogicalBytes(0),
        entities: 0,
        refinements: 0,
    });
    Census::restore(accounting, LogicalBytes(memory))
        .map(|census| Box::new(census) as Box<dyn AllocationCensus>)
        .map_err(machine_breach)
}

/// A heap fault as the worker reports it: a host outcome of the
/// activation, never the guest's.
fn heap_fault(fault: lash_kernel_doc::heap::HeapFault) -> PoolError {
    PoolError::breach(ProtocolBreach::Machine {
        detail: Detail::new(fault),
    })
}

/// The kernel's refusal of a root as the protocol carries it.
fn restore_refusal(error: OpenError) -> KernelRestoreRefusal {
    match error {
        OpenError::MissingFunction { function } => KernelRestoreRefusal::MissingFunction {
            function: function.to_string(),
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
mod tests {
    use super::*;

    /// FIG-6148: activations of one process prepare identical content once
    /// per worker, without changing charge.
    #[test]
    fn process_activations_decode_one_document_per_worker() {
        let config =
            lash_vm_client::PoolConfig::codemode(lash_vm_client::WorkerEntry::helper("unused"));
        let codec = FrameCodec::new(config.protocol.decode);
        let (pipe, mut parent) = UnixStream::pair().expect("pipe");
        let embedding =
            crate::embedding::tests::typescript_only(&config.tuning).expect("embedding");
        let mut server =
            Server::new(pipe, codec.clone(), Bootstrap::from(&config), &embedding).expect("server");
        read_frame(&mut parent, &codec, Instant::now() + Duration::from_secs(1)).expect("ready");
        let add = embedding
            .library()
            .iter()
            .find(|(name, _)| name.to_string() == "num.add")
            .map(|(_, id)| id)
            .expect("helper");
        let document = lash_kernel_doc::parse_document(&format!(
            "kernel 1\nnumbers float\nuse num.add = @{add}\nfn work() {{ let n = num.add(3, 4) return n }}\nentry work() -> Any\nmain {{ return null }}",
        )).expect("document");
        let start = Start {
            owner: VmOwner::new("process:law"),
            dialect: lash_vm_releases::TYPESCRIPT.to_owned(),
            document: wire::wrap(
                PayloadKind::Document,
                document.to_json().expect("json").as_bytes(),
            )
            .expect("document payload"),
            from: StartFrom::Fresh(
                wire::encode(
                    PayloadKind::Start,
                    &StartWire::of(
                        lash_kernel_vm::Target::Entry(lash_kernel_doc::Name::new("work")),
                        Vec::new(),
                        None,
                        &lash_kernel_vm::EffectModes::recorded(),
                        Vec::new(),
                    ),
                )
                .expect("start"),
            ),
            bounds: config.run_bounds,
            binding: [3; 32],
            save: None,
            warm: Vec::new(),
            observe_sites: false,
            observe_counts: false,
            observe_path: false,
        };
        struct Quiet;
        impl lash_kernel_vm::Host for Quiet {
            fn clock(&mut self) -> lash_kernel_doc::Timestamp {
                lash_kernel_doc::Timestamp {
                    nanoseconds: 0.into(),
                }
            }
            fn random(&mut self) -> u64 {
                0
            }
            fn read(
                &mut self,
                _: &lash_kernel_doc::Handle,
                _: &Datum,
            ) -> Result<Datum, lash_kernel_doc::ErrorDatum> {
                Ok(Datum::Null)
            }
            fn print(&mut self, _: &Datum) {}
            fn cancel_requested(&mut self) -> bool {
                false
            }
        }
        let mut first = server.start(start.clone()).expect("first activation");
        assert_eq!(
            first.machine.run(&mut Quiet, 1).expect("slice"),
            Step::Slice
        );
        let meters = first.machine.meters();
        drop(first);
        let mut second = server.start(start.clone()).expect("second activation");
        assert_eq!(
            second.machine.run(&mut Quiet, 1).expect("slice"),
            Step::Slice
        );
        assert_eq!(second.machine.meters().charged, meters.charged);
        drop(second);
        assert_eq!(
            server.document_decodes, 1,
            "two activations share one preparation"
        );
        // Another format cannot reuse the earlier document's preparation.
        let mut changed = document;
        changed.manifest.kernel = 999;
        let mut other = start.clone();
        other.document = wire::wrap(
            PayloadKind::Document,
            changed.to_json().expect("json").as_bytes(),
        )
        .expect("document payload");
        assert!(server.start(other).is_err());
        server
            .start(start)
            .expect("original preparation remains valid");
        assert_eq!(server.document_decodes, 2);
        // Distinct identities evict old residency rather than growing it.
        for n in 0..17 {
            let next = lash_kernel_doc::parse_document(&format!(
                "kernel 1\nnumbers float\nmain {{ return {n} }}",
            ))
            .expect("document");
            server
                .program(
                    &wire::wrap(
                        PayloadKind::Document,
                        next.to_json().expect("json").as_bytes(),
                    )
                    .expect("payload"),
                )
                .expect("preparation");
        }
        assert_eq!(server.documents.len(), 16);
    }

    /// FIG-5497: the worker must obey the parent's response IO deadline.
    #[test]
    fn parent_serialization_deadline_bounds_worker_response_write() {
        let mut config =
            lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"));
        config.deadlines.serialization = Duration::from_millis(5);
        let codec = FrameCodec::new(config.protocol.decode);
        let (pipe, mut parent) = UnixStream::pair().expect("pipe");
        let embedding = crate::embedding::tests::typescript_only(&config.tuning)
            .expect("a TypeScript embedding");
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
        let embedding = crate::embedding::tests::typescript_only(&config.tuning)
            .expect("a TypeScript embedding");
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
