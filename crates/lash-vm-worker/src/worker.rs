use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::PoolError;
use lash_vm_client::RunContext;
#[cfg(test)]
use lash_vm_client::ipc::read_frame;
use lash_vm_client::ipc::{Bootstrap, FrameSource, write_frame, write_frames};
use lash_vm_protocol::*;
use lashlang::{
    AbilityOp, AbilityOutcome, Entry, ExecutionBound, ExecutionBounds, ModuleArtifact,
    RuntimeError, State, VmExecutionStart, VmInstance, VmRequest, VmResume, VmRunConfig, VmStep,
};

#[derive(Default)]
struct ExchangeTiming {
    active: bool,
    started: Option<Instant>,
    serializing: Option<Instant>,
    response_started_ns: u64,
    decode_ns: std::cell::Cell<u64>,
    guest_ns: u64,
}

pub(crate) struct Server<'frontend, const MEASURE: bool = false> {
    timing: ExchangeTiming,
    frontend: &'frontend dyn crate::Frontend,
    pipe: UnixStream,
    /// The parent's frames, shared with the run's projection reads.
    inbound: Arc<Mutex<FrameSource>>,
    /// The run's projection wire, made at its start and reused by every
    /// effect answer (FIG-4433).
    wire: Option<Arc<crate::projection::Wire>>,
    codec: FrameCodec,
    bootstrap: Bootstrap,
    instance: VmInstance,
    fences: Arc<Mutex<Fences>>,
    owner: Option<VmOwner>,
    pending: Option<EffectRequest>,
    reissue: Option<RecordedRequest>,
    capture_state_view: bool,
    cpu_ceiling: Option<libc::rlim_t>,
    /// The run's heap budget, which also bounds the observations a step
    /// holds until it hands them on (FIG-4458). `None` is unbounded.
    observation_budget: Option<u64>,
}

pub(crate) struct Fences {
    pub incoming: Option<MessageFence>,
    pub outgoing: MessageFence,
    pub next_effect: u64,
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

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ParkedRun {
    pub vm: EncodedPayload,
    pub request: Option<RecordedRequest>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordedRequest {
    kind: EffectKind,
    payload: EncodedPayload,
}

impl ParkedRun {
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, PoolError> {
        rmp_serde::from_slice(bytes).map_err(|error| {
            PoolError::refused(RunRefusal::Undecodable {
                input: RunInput::State {
                    kind: VmStateKind::Continuation,
                },
                detail: Detail::new(error),
            })
        })
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, PoolError> {
        rmp_serde::to_vec_named(self)
            .map_err(|error| PoolError::payload(PayloadKind::ParkedRun, error))
    }
}

impl<'frontend, const MEASURE: bool> Server<'frontend, MEASURE> {
    pub(crate) fn new(
        pipe: UnixStream,
        codec: FrameCodec,
        bootstrap: Bootstrap,
        frontend: &'frontend dyn crate::Frontend,
    ) -> Result<Self, PoolError> {
        let mut server = Self {
            frontend,
            timing: ExchangeTiming::default(),
            pipe,
            inbound: Arc::default(),
            wire: None,
            codec,
            bootstrap,
            instance: VmInstance::pristine(),
            fences: Arc::new(Mutex::new(Fences {
                incoming: None,
                outgoing: MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0)),
                next_effect: 0,
            })),
            owner: None,
            pending: None,
            reissue: None,
            capture_state_view: false,
            cpu_ceiling: None,
            observation_budget: None,
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
            Instant::now() + Duration::from_secs(30),
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
        if MEASURE && self.timing.active && phase == WorkerPhase::Serializing {
            self.timing.response_started_ns = lash_vm_client::ipc::monotonic_nanos()?;
            self.timing.serializing = Some(Instant::now());
        }
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
                    Instant::now() + Duration::from_secs(86_400),
                );
            let bytes = match received {
                Ok(bytes) => bytes,
                Err(PoolError::Infrastructure(InfrastructureOutcome::WorkerCrashed { .. })) => {
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            if MEASURE {
                self.timing = ExchangeTiming {
                    started: Some(Instant::now()),
                    ..ExchangeTiming::default()
                };
            }
            let frame = self.codec.decode_parent(&bytes).map_err(PoolError::from)?;
            if MEASURE {
                self.timing.active = matches!(
                    &frame.message,
                    ParentMessage::Start(_) | ParentMessage::EffectResponse(_)
                );
                self.timing.decode_ns.set(
                    self.timing
                        .started
                        .map_or(0, |start| start.elapsed().as_nanos() as u64),
                );
            }
            let mut fences = self
                .fences
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if fences.incoming.is_none() {
                if !matches!(
                    frame.message,
                    ParentMessage::Start(_)
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
                    if self.owner.is_some() {
                        return Err(PoolError::breach(SequenceFault::StartBeforeReset));
                    }
                    self.owner = Some(start.owner.clone());
                    self.progress(WorkerPhase::Computing)?;
                    let step = self.start(*start)?;
                    self.deliver(step)?;
                }
                ParentMessage::Prepare { owner, request } => {
                    if self.owner.is_some() {
                        return Err(PoolError::breach(SequenceFault::PrepareBeforeReset));
                    }
                    self.owner = Some(owner);
                    self.progress(WorkerPhase::Computing)?;
                    self.codec.check_payload(&request.0)?;
                    let response =
                        crate::service::perform(self.frontend, &mut self.instance, &request)?;
                    self.progress(WorkerPhase::Serializing)?;
                    self.respond(WorkerMessage::Prepared { response })?;
                }
                ParentMessage::EffectResponse(result) => {
                    let request = self
                        .pending
                        .take()
                        .ok_or_else(|| PoolError::breach(SequenceFault::NoPendingRequest))?;
                    if result.id != request.id {
                        return Err(PoolError::breach(SequenceFault::WrongRequestId));
                    }
                    self.progress(WorkerPhase::Computing)?;
                    let resume = match (request.kind, result.outcome) {
                        (EffectKind::CancelCheckpoint, EffectOutcome::Checkpoint { cancelled }) => {
                            VmResume::CancelCheckpoint { cancelled }
                        }
                        (
                            EffectKind::ProcessBoundary | EffectKind::ParkDeclined,
                            EffectOutcome::Unit,
                        ) => VmResume::Continue,
                        (
                            EffectKind::CancelCheckpoint
                            | EffectKind::ProcessBoundary
                            | EffectKind::ParkDeclined,
                            _,
                        ) => return Err(PoolError::breach(SequenceFault::WrongControlResult)),
                        (_, EffectOutcome::Cancelled) => VmResume::EffectCancelled,
                        (_, EffectOutcome::Value(value)) => {
                            let value: AbilityOutcome = self.decode(&value)?;
                            VmResume::Effect(Ok(value))
                        }
                        (_, EffectOutcome::Unit) => VmResume::Effect(Ok(AbilityOutcome::Unit)),
                        (_, EffectOutcome::HandedOver) => {
                            VmResume::Effect(Ok(AbilityOutcome::HandedOver))
                        }
                        (_, EffectOutcome::Failed(error)) => {
                            VmResume::Effect(Err(self.decode(&error)?))
                        }
                        (_, EffectOutcome::Checkpoint { .. }) => {
                            return Err(PoolError::breach(SequenceFault::CheckpointAnsweredEffect));
                        }
                    };
                    let guest_started = (MEASURE && self.timing.active).then(Instant::now);
                    let step = self.instance.resume(resume).map_err(vm_breach)?;
                    if let Some(guest_started) = guest_started {
                        self.timing.guest_ns = guest_started.elapsed().as_nanos() as u64;
                    }
                    self.deliver(step)?;
                }
                ParentMessage::Park => {
                    // A process boundary, or an effect the run can issue again
                    // once its continuation is resumed (FIG-4159).
                    if !self
                        .pending
                        .as_ref()
                        .is_some_and(|request| request.kind.parkable())
                    {
                        return Err(PoolError::breach(SequenceFault::ParkWithoutParkableRequest));
                    }
                    self.progress(WorkerPhase::Computing)?;
                    let step = self.instance.resume(VmResume::Park).map_err(vm_breach)?;
                    self.deliver(step)?;
                }
                ParentMessage::Cancel => {
                    // Physical cancellation never decides the journaled winner.
                    self.instance.reset();
                    self.pending = None;
                    self.reissue = None;
                    self.wire = None;
                    self.send(WorkerMessage::Cancelled)?;
                }
                ParentMessage::Reset => {
                    self.cpu_ceiling = None;
                    self.capture_state_view = false;
                    self.observation_budget = None;
                    self.instance.reset();
                    self.pending = None;
                    self.reissue = None;
                    self.wire = None;
                    self.owner = None;
                    self.fences
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .next_effect = 0;
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
    /// The run's projection wire, on this socket and under this run's fences.
    fn wire(&mut self) -> Result<Arc<crate::projection::Wire>, PoolError> {
        if let Some(wire) = &self.wire {
            return Ok(wire.clone());
        }
        let wire = Arc::new(crate::projection::Wire::new(
            self.pipe.try_clone().map_err(PoolError::io)?,
            self.inbound.clone(),
            self.codec.clone(),
            self.fences.clone(),
        ));
        self.wire = Some(wire.clone());
        Ok(wire)
    }
    fn decode<T: serde::de::DeserializeOwned>(
        &self,
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
        let measured = (MEASURE && self.timing.active).then(Instant::now);
        self.codec.check_payload(&payload.0)?;
        let result = rmp_serde::from_slice(&payload.0)
            .map_err(|error| PoolError::payload(PayloadKind::EffectOutcome, error));
        if let Some(measured) = measured {
            self.timing
                .decode_ns
                .set(self.timing.decode_ns.get() + measured.elapsed().as_nanos() as u64);
        }
        result
    }
    fn start(&mut self, start: Start) -> Result<VmStep, PoolError> {
        let mut context = RunContext::default();
        for description in &start.contexts {
            if description.kind != "vm_run" {
                return Err(PoolError::refused(RunRefusal::UnknownContext));
            }
            self.codec.check_payload(&description.body.0)?;
            context = rmp_serde::from_slice(&description.body.0).map_err(|error| {
                PoolError::refused(RunRefusal::Undecodable {
                    input: RunInput::Context,
                    detail: Detail::new(error),
                })
            })?;
        }
        self.wire = None;
        self.capture_state_view = context.capture_state_view;
        self.observation_budget = start.limits.memory_limit_bytes;
        let execution_start = match start.state {
            StartState::Fresh => VmExecutionStart::Session,
            StartState::Snapshot(state) => {
                self.check(&state, VmStateKind::Snapshot)?;
                let snapshot = self
                    .instance
                    .open_snapshot(state.bytes())
                    .map_err(|error| undecodable_state(VmStateKind::Snapshot, error))?;
                self.instance.replace_state(State::from_snapshot(snapshot));
                VmExecutionStart::Session
            }
            StartState::Continuation(state) => {
                self.check(&state, VmStateKind::Continuation)?;
                let parked = ParkedRun::decode(state.bytes())?;
                if let Some(request) = &parked.request {
                    self.codec.check_payload(&request.payload.0)?;
                }
                self.reissue = parked.request;
                VmExecutionStart::Continuation(Box::new(
                    self.instance
                        .open_continuation(&parked.vm.0)
                        .map_err(|error| undecodable_state(VmStateKind::Continuation, error))?,
                ))
            }
        };
        let program = match start.program {
            ProgramSource::Source { dialect, text } => {
                if dialect != self.frontend.language_id() {
                    return Err(PoolError::refused(RunRefusal::SourceDialect));
                }
                if text.len() as u64 > self.bootstrap.source {
                    return Err(PoolError::refused(RunRefusal::PayloadTooLarge {
                        limit: self.bootstrap.source,
                        size: text.len() as u64,
                    }));
                }
                let ast = self
                    .frontend
                    .parse(&text, Some(&context.environment))
                    .map_err(|refusal| {
                        PoolError::refused(RunRefusal::Parse {
                            detail: Detail::new(refusal.error),
                        })
                    })?;
                let linked = self
                    .instance
                    .linked_programs_mut()
                    .get_or_compile_ast(&text, ast, &context.environment)
                    .map_err(compile_refusal)?;
                linked.compiled_program().clone()
            }
            ProgramSource::Artifact {
                module_ref,
                entry,
                artifact,
            } => {
                let artifact = ModuleArtifact::from_store_bytes(&artifact).map_err(|error| {
                    PoolError::refused(RunRefusal::Undecodable {
                        input: RunInput::Artifact,
                        detail: Detail::new(error),
                    })
                })?;
                if artifact.module_ref().as_str() != module_ref {
                    return Err(PoolError::refused(RunRefusal::ArtifactIdentityMismatch));
                }
                let process_ref;
                let entry = match entry {
                    ProgramEntry::Main => Entry::Main,
                    ProgramEntry::Process {
                        component,
                        position,
                    } => {
                        process_ref = lashlang::ProcessRef::new(
                            lashlang::ContentHash::new(component),
                            position,
                        );
                        Entry::Process(&process_ref)
                    }
                };
                lashlang::compile(&artifact, entry, None).map_err(compile_refusal)?
            }
        };
        let bound = |v: Option<u64>| -> Result<ExecutionBound<std::num::NonZeroU64>, PoolError> {
            match v {
                None => Ok(ExecutionBound::Unbounded),
                Some(v) => std::num::NonZeroU64::new(v)
                    .map(ExecutionBound::Bounded)
                    .ok_or_else(|| PoolError::refused(RunRefusal::ZeroLimit)),
            }
        };
        let depth = std::num::NonZeroU64::new(start.limits.max_frame_depth)
            .ok_or_else(|| PoolError::refused(RunRefusal::ZeroLimit))?;
        let mut config = VmRunConfig::new(
            context.mode,
            ExecutionBounds::new(
                bound(start.limits.instruction_budget)?,
                bound(start.limits.memory_limit_bytes)?,
            )
            .with_max_frame_depth(depth),
        );
        config.observe_execution = context.observe_execution;
        config.trace_runtime_errors = true;
        for description in context.projected {
            let lashlang::Value::Projected(value) = description.value else {
                return Err(PoolError::refused(RunRefusal::ProjectedBinding {
                    detail: Detail::new(format!(
                        "projected binding `{}` is not a projection",
                        description.name
                    )),
                }));
            };
            config
                .projected
                .try_insert(description.name, value)
                .map_err(|error| {
                    PoolError::refused(RunRefusal::ProjectedBinding {
                        detail: Detail::new(error),
                    })
                })?;
        }
        config.projected =
            config
                .projected
                .with_reader(Arc::new(crate::projection::RemoteProjection {
                    wire: self.wire()?,
                }));
        self.instance
            .start(Arc::new(program), execution_start, config)
            .map_err(|error| {
                PoolError::refused(RunRefusal::Start {
                    detail: Detail::new(error),
                })
            })
    }
    fn check(&self, state: &OpaqueVmState, kind: VmStateKind) -> Result<(), PoolError> {
        state
            .check(&StateExpectation {
                kind,
                owner: self
                    .owner
                    .as_ref()
                    .ok_or_else(|| PoolError::breach(SequenceFault::MissingOwner))?,
                reads: &lashlang::vm_contract_reads(),
                max_bytes: self.bootstrap.state,
            })
            .map_err(|refusal| InfrastructureOutcome::input_state(refusal).into())
    }
    fn seal(&self, kind: VmStateKind, bytes: Vec<u8>) -> Result<OpaqueVmState, PoolError> {
        if bytes.len() as u64 > self.bootstrap.state {
            return Err(InfrastructureOutcome::WorkerLimitExceeded {
                limit: WorkerLimit::VmState {
                    size: bytes.len() as u64,
                    bound: self.bootstrap.state,
                },
            }
            .into());
        }
        Ok(OpaqueVmState::seal(
            kind,
            self.owner
                .clone()
                .ok_or_else(|| PoolError::breach(SequenceFault::MissingOwner))?,
            lashlang::vm_contract_versions(),
            bytes,
        )
        .with_definition_ids(self.instance.state().referenced_definition_ids()))
    }
    fn snapshot(&self) -> Result<OpaqueVmState, PoolError> {
        self.seal(
            VmStateKind::Snapshot,
            self.instance
                .state()
                .snapshot()
                .to_canonical_bytes()
                .map_err(|error| PoolError::payload(PayloadKind::Snapshot, error))?,
        )
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
        if MEASURE && self.timing.active {
            fence.next_header();
        }
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
        let encode_ns = if MEASURE && self.timing.active {
            self.timing
                .serializing
                .map_or(0, |start| start.elapsed().as_nanos() as u64)
        } else {
            0
        };
        // Responding and the answer leave in one write: nothing happens
        // between them, and the parent then wakes once for both (FIG-4433).
        let responding = {
            let mut fences = self
                .fences
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let telemetry = if MEASURE && self.timing.active {
                Some(fences.encode(
                    &self.codec,
                    WorkerMessage::ExchangeTiming {
                        response_started_ns: self.timing.response_started_ns,
                        decode_ns: self.timing.decode_ns.get(),
                        encode_ns,
                        guest_ns: self.timing.guest_ns,
                    },
                )?)
            } else {
                None
            };
            let responding = fences.encode(
                &self.codec,
                WorkerMessage::Progress {
                    phase: WorkerPhase::Responding,
                    cpu_nanos: cpu_nanos()?,
                },
            )?;
            fences.outgoing.next_header();
            if let Some(mut bytes) = telemetry {
                bytes.extend_from_slice(&responding);
                bytes
            } else {
                responding
            }
        };
        write_frames(&mut self.pipe, responding, &bytes, Duration::from_secs(30))?;
        Ok(message)
    }
    fn deliver(&mut self, step: VmStep) -> Result<(), PoolError> {
        let message = match self.deliver_inner(step) {
            Err(PoolError::Infrastructure(InfrastructureOutcome::WorkerLimitExceeded {
                limit,
            })) => WorkerMessage::LimitExceeded { limit },
            result => result?,
        };
        // The request is kept as it was sent, without a copy of its payload.
        if let WorkerMessage::EffectRequest(request) = self.respond(message)? {
            self.pending = Some(request);
        }
        Ok(())
    }
    /// A step's observations as the payloads of the frames that carry them,
    /// each within the frame's bounds (FIG-4458); `None` when they outgrow
    /// the run's heap budget, or one alone outgrows a frame.
    fn observation_chunks(
        &self,
        observations: &[lashlang::LashlangExecutionObservation],
    ) -> Result<Option<Vec<EncodedPayload>>, PoolError> {
        if observations.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let mut chunker = self.codec.observation_chunker()?;
        for observation in observations {
            let encoded = rmp_serde::to_vec_named(observation)
                .map_err(|error| PoolError::payload(PayloadKind::Observation, error))?;
            match chunker.push(&encoded) {
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
            if self
                .observation_budget
                .is_some_and(|budget| chunker.bytes() > budget)
            {
                return Ok(None);
            }
        }
        Ok(Some(chunker.finish()))
    }
    fn deliver_inner(&mut self, mut step: VmStep) -> Result<WorkerMessage, PoolError> {
        // Lazy reads use the compute phase and the same request fence.
        if let VmStep::Complete(complete) = &mut step {
            complete.outcome = materialize_outcome(complete.outcome.clone())?;
        }
        self.progress(WorkerPhase::Serializing)?;
        let observations = match &step {
            VmStep::Suspended(step) => &step.observations,
            VmStep::Parked(step) => &step.observations,
            VmStep::Complete(step) => &step.observations,
            VmStep::GuestError(step) => &step.observations,
        };
        let Some(chunks) = self.observation_chunks(observations)? else {
            return Ok(WorkerMessage::LimitExceeded {
                limit: WorkerLimit::Observations,
            });
        };
        for payload in chunks {
            self.send(WorkerMessage::Observations { payload })?;
        }
        let message = match step {
            VmStep::Suspended(suspended) => {
                let (kind, payload) = match suspended.request {
                    VmRequest::Effect(op) => {
                        let kind = match &op {
                            AbilityOp::ResourceOperation(_) => EffectKind::ResourceOperation,
                            AbilityOp::ResourceOperationBatch(_) => {
                                EffectKind::ResourceOperationBatch
                            }
                            AbilityOp::Await(_) => EffectKind::Await,
                            AbilityOp::Print(_) => EffectKind::Print,
                            AbilityOp::Finish(_) => EffectKind::Finish,
                            AbilityOp::Fail(_) => EffectKind::Fail,
                            AbilityOp::ProcessEvent(_) => EffectKind::ProcessEvent,
                            AbilityOp::Sleep(_) => EffectKind::Sleep,
                            AbilityOp::WaitSignal { .. } => EffectKind::WaitSignal,
                        };
                        // The VM wire carries a scalar projection as its value,
                        // so a request rebuilt from the continuation is not
                        // the one already issued: resume reads that request.
                        match self.reissue.take() {
                            Some(recorded) if recorded.kind == kind => {
                                (recorded.kind, recorded.payload.0)
                            }
                            Some(_) => {
                                return Err(PoolError::refused(RunRefusal::ParkedRequestChanged));
                            }
                            None => (
                                kind,
                                rmp_serde::to_vec_named(&op).map_err(|error| {
                                    PoolError::payload(PayloadKind::EffectRequest, error)
                                })?,
                            ),
                        }
                    }
                    VmRequest::CancelCheckpoint(n) => (
                        EffectKind::CancelCheckpoint,
                        rmp_serde::to_vec_named(&n).map_err(|error| {
                            PoolError::payload(PayloadKind::CancelCheckpoint, error)
                        })?,
                    ),
                    VmRequest::Boundary => (EffectKind::ProcessBoundary, Vec::new()),
                    VmRequest::ParkDeclined(error) => (
                        EffectKind::ParkDeclined,
                        rmp_serde::to_vec_named(&error.to_string())
                            .map_err(|error| PoolError::payload(PayloadKind::ParkDecline, error))?,
                    ),
                };
                if payload.len() as u64 > self.bootstrap.effect {
                    return Err(InfrastructureOutcome::WorkerLimitExceeded {
                        limit: WorkerLimit::EffectValue {
                            size: payload.len() as u64,
                            bound: self.bootstrap.effect,
                        },
                    }
                    .into());
                }
                let id = {
                    let mut fences = self
                        .fences
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let id = EffectRequestId(fences.next_effect);
                    fences.next_effect += 1;
                    id
                };
                WorkerMessage::EffectRequest(EffectRequest {
                    id,
                    kind,
                    payload: EncodedPayload(payload),
                })
            }
            VmStep::Parked(parked) => {
                let bytes =
                    ParkedRun {
                        vm: EncodedPayload(parked.continuation.to_bytes().map_err(|error| {
                            PoolError::payload(PayloadKind::Continuation, error)
                        })?),
                        request: self
                            .pending
                            .take()
                            .filter(|request| request.kind != EffectKind::ProcessBoundary)
                            .map(|request| RecordedRequest {
                                kind: request.kind,
                                payload: request.payload,
                            }),
                    }
                    .encode()?;
                WorkerMessage::Suspended {
                    state: self
                        .seal(VmStateKind::Continuation, bytes)?
                        .with_definition_ids(parked.continuation.referenced_definition_ids()),
                }
            }
            VmStep::Complete(complete) => {
                let state = self.snapshot()?;
                let value = if self.capture_state_view {
                    rmp_serde::to_vec_named(&lash_vm_client::service::CellCompletion {
                        outcome: complete.outcome,
                        state: EncodedPayload(
                            rmp_serde::to_vec_named(&crate::service::state_metadata(
                                &self.instance,
                            ))
                            .map_err(|error| PoolError::payload(PayloadKind::Completion, error))?,
                        ),
                    })
                } else {
                    rmp_serde::to_vec_named(&complete.outcome)
                }
                .map_err(|error| PoolError::payload(PayloadKind::Completion, error))?;
                WorkerMessage::Complete {
                    state,
                    value: EncodedPayload(value),
                }
            }
            VmStep::GuestError(error) => {
                let limit = match error.failure.error {
                    RuntimeError::InstructionBudgetExceeded { .. }
                    | RuntimeError::RegExpBudgetExceeded { .. } => Some(WorkerLimit::Fuel),
                    RuntimeError::MemoryLimitExceeded { .. } => Some(WorkerLimit::Heap),
                    RuntimeError::FrameDepthExceeded { .. } => Some(WorkerLimit::Depth),
                    _ => None,
                };
                match limit {
                    Some(limit) => WorkerMessage::LimitExceeded { limit },
                    None => WorkerMessage::GuestError {
                        state: Some(self.snapshot()?),
                        error: EncodedPayload(
                            rmp_serde::to_vec_named(&error.failure).map_err(|error| {
                                PoolError::payload(PayloadKind::GuestError, error)
                            })?,
                        ),
                    },
                }
            }
        };
        Ok(message)
    }
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

/// How deep a run's terminal value may nest.
const TERMINAL_VALUE_DEPTH: usize = 64;

fn vm_breach(error: impl std::fmt::Display) -> PoolError {
    PoolError::breach(ProtocolBreach::Vm {
        detail: Detail::new(error),
    })
}

fn undecodable_state(kind: VmStateKind, error: impl std::fmt::Display) -> PoolError {
    PoolError::refused(RunRefusal::Undecodable {
        input: RunInput::State { kind },
        detail: Detail::new(error),
    })
}

fn compile_refusal(error: impl std::fmt::Display) -> PoolError {
    PoolError::refused(RunRefusal::Compile {
        detail: Detail::new(error),
    })
}

fn materialize_outcome(
    outcome: lashlang::ExecutionOutcome,
) -> Result<lashlang::ExecutionOutcome, PoolError> {
    Ok(match outcome {
        lashlang::ExecutionOutcome::Finished(value) => {
            lashlang::ExecutionOutcome::Finished(materialize(value, 0)?)
        }
        lashlang::ExecutionOutcome::Failed(value) => {
            lashlang::ExecutionOutcome::Failed(materialize(value, 0)?)
        }
        other => other,
    })
}
fn materialize(value: lashlang::Value, depth: usize) -> Result<lashlang::Value, PoolError> {
    use lashlang::{Record, Value};
    if depth > TERMINAL_VALUE_DEPTH {
        return Err(PoolError::refused(RunRefusal::ValueTooDeep {
            limit: TERMINAL_VALUE_DEPTH as u32,
        }));
    }
    Ok(match value {
        Value::Projected(value) => Value::Projected(lashlang::ProjectedValue::scalar(
            value.name().to_owned(),
            materialize(
                value
                    .materialize()
                    .map_err(|error| PoolError::payload(PayloadKind::ProjectedValue, error))?,
                depth + 1,
            )?,
        )),
        Value::List(values) => Value::List(
            values
                .iter()
                .cloned()
                .map(|value| materialize(value, depth + 1))
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        ),
        Value::Tuple(values) => Value::Tuple(
            values
                .iter()
                .cloned()
                .map(|value| materialize(value, depth + 1))
                .collect::<Result<Vec<_>, _>>()?
                .into(),
        ),
        Value::Record(values) => Value::Record(Arc::new(
            values
                .iter()
                .map(|(key, value)| {
                    materialize(value.clone(), depth + 1).map(|value| (key.to_string(), value))
                })
                .collect::<Result<Record, _>>()?,
        )),
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oversized_non_observation_frame_preserves_its_fence_and_typed_cause() {
        let mut config =
            lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"));
        config.protocol.decode.max_frame_bytes = 1024;
        let codec = FrameCodec::new(config.protocol.decode);
        let (pipe, mut parent) = UnixStream::pair().expect("pipe");
        let frontend = crate::frontend::TypeScriptFrontend::default();
        let mut server =
            Server::<false>::new(pipe, codec.clone(), Bootstrap::from(&config), &frontend)
                .expect("server");
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

    /// Runs `effects` echo effects through a server on its own thread, and
    /// answers the socket calls that thread made.
    fn server_socket_calls(effects: usize) -> u64 {
        let config =
            lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"));
        let codec = FrameCodec::new(config.protocol.decode);
        let (pipe, mut parent) = UnixStream::pair().expect("pipe");
        let server = std::thread::spawn({
            let codec = codec.clone();
            let bootstrap = Bootstrap::from(&config);
            move || {
                let frontend = crate::frontend::TypeScriptFrontend::default();
                let mut server =
                    Server::<false>::new(pipe, codec, bootstrap, &frontend).expect("server");
                server.run(&mut None).expect("run");
                lash_vm_client::ipc::socket_calls()
            }
        });
        let deadline = || Instant::now() + Duration::from_secs(30);
        let mut outgoing = MessageFence::new(ExecutionLease(1), OwnerEpoch(1), FrameEpoch(1));
        let mut send = |parent: &mut UnixStream, message| {
            let bytes = codec
                .encode_parent(&ParentFrame {
                    header: outgoing.next_header(),
                    message,
                })
                .expect("parent frame");
            write_frame(parent, &bytes, deadline()).expect("send");
        };
        let receive = |parent: &mut UnixStream| {
            let bytes = read_frame(parent, &codec, deadline()).expect("worker frame");
            codec.decode_worker(&bytes).expect("frame").message
        };
        assert!(matches!(receive(&mut parent), WorkerMessage::Ready { .. }));
        send(
            &mut parent,
            ParentMessage::Start(Box::new(Start {
                owner: VmOwner::new("session"),
                program: ProgramSource::Source {
                    dialect: "typescript".into(),
                    text: format!(
                        "for (let i = 0; i < {effects}; i++) {{ await tools.echo({{ value: i }}); }} finish(1);"
                    ),
                },
                contexts: vec![ContextDescription {
                    kind: "vm_run".into(),
                    name: "context".into(),
                    body: EncodedPayload(
                        rmp_serde::to_vec_named(&RunContext {
                            environment: lashlang::testing::harness::test_environment(),
                            ..RunContext::default()
                        })
                        .expect("context"),
                    ),
                }],
                state: StartState::Fresh,
                limits: config.vm_limits,
            })),
        );
        let mut answered = 0;
        loop {
            match receive(&mut parent) {
                WorkerMessage::Progress { .. } => {}
                WorkerMessage::EffectRequest(request) => {
                    let outcome = match request.kind {
                        EffectKind::CancelCheckpoint => {
                            EffectOutcome::Checkpoint { cancelled: false }
                        }
                        EffectKind::ResourceOperation => {
                            answered += 1;
                            EffectOutcome::Value(EncodedPayload(
                                rmp_serde::to_vec_named(&AbilityOutcome::Value(
                                    lashlang::Value::Number(1.0),
                                ))
                                .expect("answer"),
                            ))
                        }
                        EffectKind::Finish => {
                            let AbilityOp::Finish(value) =
                                rmp_serde::from_slice(&request.payload.0).expect("operation")
                            else {
                                panic!("a finish request carries its value")
                            };
                            EffectOutcome::Value(EncodedPayload(
                                rmp_serde::to_vec_named(&AbilityOutcome::Value(value))
                                    .expect("answer"),
                            ))
                        }
                        other => panic!("unexpected effect {other:?}"),
                    };
                    send(
                        &mut parent,
                        ParentMessage::EffectResponse(EffectResponse {
                            id: request.id,
                            outcome,
                        }),
                    );
                }
                WorkerMessage::Complete { .. } => break,
                other => panic!("expected Complete, received {other:?}"),
            }
        }
        assert_eq!(answered, effects);
        send(&mut parent, ParentMessage::Shutdown);
        server.join().expect("server thread")
    }

    /// FIG-4433: one effect exchange costs the worker five socket calls: a
    /// timeout and a read for the whole answer, then one write each for
    /// Computing, Serializing, and Responding together with the next request.
    #[test]
    fn an_effect_exchange_costs_the_worker_five_socket_calls() {
        const MORE: usize = 10;
        let few = server_socket_calls(1);
        let many = server_socket_calls(1 + MORE);
        assert!(
            many - few <= 5 * MORE as u64,
            "{MORE} more effect exchanges cost the worker {} more socket calls",
            many - few
        );
    }
}

#[cfg(test)]
#[path = "../tests/performance/mod.rs"]
mod performance_tests;
