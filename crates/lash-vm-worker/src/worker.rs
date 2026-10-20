use std::collections::VecDeque;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use crate::PoolError;
use crate::embedding::Embedding;
use crate::host::{PagePort, Wire, WireHost, WireLimiter, WirePageSource, grant_of, link_grant};
use lash_kernel_doc::heap::{PageId, PageRef};
use lash_kernel_doc::{Datum, IdentifiedDocument};
use lash_kernel_state::paged::{Checkpoint, PageSource};
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
    /// At most sixteen documents and eight MiB of token bodies are held;
    /// a larger document runs normally without becoming resident.
    documents: VecDeque<(EncodedPayload, Program)>,
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
        // creation. A worker whose host did not name that dialect refuses
        // before it reads the document or the checkpoint (FIG-5990).
        if !self.embedding.installs(&start.dialect) {
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
        let program = self.program(&start.document, fresh)?;
        let path = start.observe_path.then(|| program.document.site_ordinals());
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
        let captions = save
            .as_ref()
            .map(|save| lash_kernel_dialect::function_captions(&program.document, &save.written))
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
                    Box::new(source),
                )
                .map_err(|error| match error {
                    lash_kernel_vm::StartError::Heap(fault) => heap_fault(fault),
                    error @ lash_kernel_vm::StartError::Accounting { .. } => machine_breach(error),
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
                KernelMachine::open(program, bounds, &root, Box::new(source)).map_err(|error| {
                    match error {
                        OpenError::Heap(fault) => heap_fault(fault),
                        error @ OpenError::Accounting { .. } => machine_breach(error),
                        error => PoolError::refused(RunRefusal::Restore {
                            refusal: restore_refusal(error),
                        }),
                    }
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
    /// The content key is the token body itself rather than a claimed
    /// identity. Holding the same Arc also reuses PreparedLibrary's
    /// admission and code; eviction lets its weak document entries be
    /// reclaimed on admission. A miss decodes the body once, under the
    /// worker's data limits, and derives its execution closure over the
    /// worker's registry: a resume needs every function the closure lists,
    /// and the native of each native-only one (`K-MACH-008`). A `fresh`
    /// start that reaches a function the worker lacks is refused as a start
    /// is, and a resume as a restore, naming the function.
    fn program(&mut self, encoded: &EncodedPayload, fresh: bool) -> Result<Program, PoolError> {
        if let Some(index) = self
            .documents
            .iter()
            .position(|(bytes, _)| bytes == encoded)
            && let Some(hit) = self.documents.remove(index)
        {
            let program = hit.1.clone();
            self.documents.push_back(hit);
            return Ok(program);
        }
        let document = IdentifiedDocument::decode(&encoded.0, self.bootstrap.tuning.kernel_data)
            .map_err(|error| {
                PoolError::refused(RunRefusal::Undecodable {
                    input: RunInput::Document,
                    detail: Detail::new(error),
                })
            })?;
        let library = self.embedding.prepared_library().map_err(|error| {
            PoolError::breach(ProtocolBreach::Machine {
                detail: Detail::new(error),
            })
        })?;
        let closure = lash_vm_broker::kernel::closure::derive(
            document.manifest.functions.keys().copied(),
            library.registry(),
            lash_vm_broker::kernel::closure::Natives::Required,
        )
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
        );
        let program = Program {
            closure: PageId(closure.reference().0),
            document: Arc::clone(document.shared()),
            library: library.clone(),
        };
        const BYTES: usize = 8 * 1024 * 1024;
        if encoded.0.len() <= BYTES {
            while self.documents.len() >= 16
                || self
                    .documents
                    .iter()
                    .map(|(bytes, _)| bytes.0.len())
                    .sum::<usize>()
                    + encoded.0.len()
                    > BYTES
            {
                self.documents.pop_front();
            }
            self.documents.push_back((encoded.clone(), program.clone()));
        }
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

/// A heap fault the worker met itself, as the protocol carries it: a host
/// outcome of the activation, never the guest's (FIG-6422).
fn heap_fault(fault: lash_kernel_doc::heap::HeapFault) -> PoolError {
    use lash_kernel_doc::heap::{HeapFault as Kernel, HostResource as KernelResource};
    let detail = || Detail::new(&fault);
    let fault = match &fault {
        Kernel::Missing { .. } => page::HeapFault::Missing { detail: detail() },
        Kernel::Corrupt { .. } => page::HeapFault::Corrupt { detail: detail() },
        Kernel::Unauthorized { .. } => page::HeapFault::Unauthorized { detail: detail() },
        Kernel::LeaseLost => page::HeapFault::LeaseLost,
        Kernel::Exhausted { resource } => page::HeapFault::Exhausted {
            resource: match resource {
                KernelResource::TraceBudget => page::HostResource::TraceBudget,
                KernelResource::CleanResidency => page::HostResource::CleanResidency,
                KernelResource::DirtyResidency => page::HostResource::DirtyResidency,
                KernelResource::PinnedPages => page::HostResource::PinnedPages,
                KernelResource::StagingBytes => page::HostResource::StagingBytes,
                KernelResource::ReadBatch => page::HostResource::ReadBatch,
                KernelResource::HostWork => page::HostResource::HostWork,
            },
        },
        Kernel::Transport(_) => page::HeapFault::Transport { detail: detail() },
        Kernel::Malformed(_) => page::HeapFault::Malformed { detail: detail() },
        _ => page::HeapFault::Malformed { detail: detail() },
    };
    InfrastructureOutcome::HeapFault { fault }.into()
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

    /// A bound server and its parent's pipe, with preparation complete.
    fn ready_server() -> (
        lash_vm_client::PoolConfig,
        FrameCodec,
        &'static Embedding,
        Server<'static>,
        UnixStream,
    ) {
        let config =
            lash_vm_client::PoolConfig::codemode(lash_vm_client::WorkerEntry::helper("unused"));
        let codec = FrameCodec::new(config.protocol.decode);
        let (pipe, parent) = UnixStream::pair().expect("pipe");
        let embedding: &'static Embedding = Box::leak(Box::new(
            crate::embedding::tests::typescript_only(&config.tuning).expect("embedding"),
        ));
        let server =
            Server::new(pipe, codec.clone(), Bootstrap::from(&config), embedding).expect("server");
        (config, codec, embedding, server, parent)
    }

    /// The `work` document and its `Start` the tests share.
    fn work_start(
        config: &lash_vm_client::PoolConfig,
        embedding: &Embedding,
    ) -> (lash_kernel_doc::Document, Start) {
        let add = embedding
            .library()
            .iter()
            .find(|(name, _)| *name == "num.add")
            .map(|(_, id)| id)
            .expect("helper");
        let document = lash_kernel_doc::parse_document(&format!(
            "kernel 1\nnumbers float\nuse num.add = @{add}\nfn work() {{ let n = num.add(3, 4) return n }}\nentry work() -> Any\nmain {{ return null }}",
        )).expect("document");
        let start = Start {
            owner: VmOwner::new("process:law"),
            dialect: lash_vm_releases::TYPESCRIPT.to_owned(),
            document: EncodedPayload(document.canonical_bytes()),
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
            memory_limiter: false,
            charge_limiter: false,
        };
        (document, start)
    }

    /// FIG-6148: activations of one process prepare identical content once
    /// per worker, without changing charge. FIG-6257: the one preparation
    /// is one token decode of the `Start`'s body, and no JSON.
    #[test]
    fn process_activations_decode_one_document_per_worker() {
        let (config, _codec, embedding, mut server, _parent) = ready_server();
        let (document, start) = work_start(&config, embedding);
        let _ = lash_kernel_doc::document_work::take();
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
            lash_kernel_doc::document_work::take(),
            lash_kernel_doc::document_work::DocumentWork {
                token_decodes: 1,
                ..Default::default()
            },
            "two activations share one token decode"
        );
        // Another format cannot reuse the earlier document's preparation.
        let mut changed = document;
        changed.manifest.kernel = 999;
        let mut other = start.clone();
        other.document = EncodedPayload(changed.canonical_bytes());
        let _ = lash_kernel_doc::document_work::take();
        assert!(server.start(other).is_err());
        server
            .start(start)
            .expect("original preparation remains valid");
        assert_eq!(lash_kernel_doc::document_work::take().token_decodes, 1);
        // Distinct identities evict old residency rather than growing it.
        for n in 0..17 {
            let next = lash_kernel_doc::parse_document(&format!(
                "kernel 1\nnumbers float\nmain {{ return {n} }}",
            ))
            .expect("document");
            server
                .program(&EncodedPayload(next.canonical_bytes()), true)
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

    /// FIG-6422: a corrupt page is permanent wherever it is detected: the
    /// worker's own heap fault crosses the pipe as itself and ends the run
    /// as terminally as the parent's `PageRefusal::Corrupt`, never
    /// redriven.
    #[test]
    fn a_corrupt_page_is_permanent_wherever_it_is_detected() {
        let (config, codec, embedding, mut server, mut parent) = ready_server();
        let (_document, mut start) = work_start(&config, embedding);
        start.warm = vec![page::ReadPage {
            page: page::PageEnvelope::new(vec![0xde, 0xad]),
            children: Vec::new(),
        }];
        let error = server
            .start(start)
            .err()
            .expect("a corrupt warm page faults the start");
        server.refuse(&error).expect("typed refusal frame");
        let bytes = read_frame(&mut parent, &codec, Instant::now() + Duration::from_secs(1))
            .expect("next frame");
        let WorkerMessage::Refused { refusal } =
            codec.decode_worker(&bytes).expect("frame").message
        else {
            panic!("a heap fault crosses as a refusal")
        };
        let outcome = refusal.into_outcome();
        assert_eq!(outcome, error.into_outcome());
        let json = serde_json::to_value(&outcome).expect("json");
        assert!(
            json.pointer("/heap_fault/fault/corrupt").is_some(),
            "{json}: the worker's own corrupt page crosses as its heap fault"
        );
        for outcome in [
            outcome,
            InfrastructureOutcome::PageRefused {
                refusal: page::PageRefusal::Corrupt,
            },
        ] {
            assert!(!outcome.is_retryable(), "{outcome:?} is permanent");
            assert!(
                !PoolError::from(outcome.clone()).is_host_verdict(),
                "{outcome:?} is no host verdict"
            );
        }
    }

    /// FIG-6422: a host-resource exhaustion crosses as the heap's typed
    /// fault, not a machine breach.
    #[test]
    fn a_trace_budget_exhaustion_crosses_as_its_typed_heap_fault() {
        let (_config, codec, _embedding, mut server, mut parent) = ready_server();
        let error = heap_fault(lash_kernel_doc::heap::HeapFault::Exhausted {
            resource: lash_kernel_doc::heap::HostResource::TraceBudget,
        });
        server.refuse(&error).expect("typed refusal frame");
        let bytes = read_frame(&mut parent, &codec, Instant::now() + Duration::from_secs(1))
            .expect("next frame");
        let WorkerMessage::Refused { refusal } =
            codec.decode_worker(&bytes).expect("frame").message
        else {
            panic!("a heap fault crosses as a refusal")
        };
        assert_eq!(
            serde_json::to_value(refusal.into_outcome()).expect("json"),
            serde_json::json!({"heap_fault": {"fault": {"exhausted": {"resource": "trace_budget"}}}})
        );
    }
}
