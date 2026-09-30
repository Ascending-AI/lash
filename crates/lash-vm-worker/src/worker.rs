use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::PoolError;
use lash_vm_client::RunContext;
use lash_vm_client::ipc::{Bootstrap, read_frame, write_frame};
use lash_vm_protocol::*;
use lashlang::{
    AbilityOp, AbilityOutcome, Entry, ExecutionBound, ExecutionBounds, ModuleArtifact,
    RuntimeError, State, VmExecutionStart, VmInstance, VmRequest, VmResume, VmRunConfig, VmStep,
};

pub(crate) struct Server<'frontend> {
    frontend: &'frontend dyn crate::Frontend,
    pipe: UnixStream,
    codec: FrameCodec,
    bootstrap: Bootstrap,
    instance: VmInstance,
    fences: Arc<Mutex<Fences>>,
    owner: Option<VmOwner>,
    pending: Option<EffectRequest>,
    reissue: Option<RecordedRequest>,
    projection_namespace: String,
    cpu_ceiling: Option<libc::rlim_t>,
}

pub(crate) struct Fences {
    pub incoming: Option<MessageFence>,
    pub outgoing: MessageFence,
    pub next_effect: u64,
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
        rmp_serde::from_slice(bytes).map_err(PoolError::protocol)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, PoolError> {
        rmp_serde::to_vec_named(self).map_err(PoolError::protocol)
    }
}

impl<'frontend> Server<'frontend> {
    pub(crate) fn new(
        pipe: UnixStream,
        codec: FrameCodec,
        bootstrap: Bootstrap,
        build: BuildIdentity,
        frontend: &'frontend dyn crate::Frontend,
    ) -> Result<Self, PoolError> {
        let mut server = Self {
            frontend,
            pipe,
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
            projection_namespace: String::new(),
            cpu_ceiling: None,
        };
        server.send(WorkerMessage::Ready { build })?;
        Ok(server)
    }

    fn send(&mut self, message: WorkerMessage) -> Result<(), PoolError> {
        let bytes = self
            .codec
            .encode_worker(&WorkerFrame {
                header: self
                    .fences
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .outgoing
                    .next_header(),
                message,
            })
            .map_err(PoolError::from)?;
        write_frame(
            &mut self.pipe,
            &bytes,
            Instant::now() + Duration::from_secs(30),
        )
    }
    pub(crate) fn refuse(&mut self, reason: String) -> Result<(), PoolError> {
        self.send(WorkerMessage::Refused { reason })
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
            let bytes = match read_frame(
                &mut self.pipe,
                &self.codec,
                Instant::now() + Duration::from_secs(86_400),
            ) {
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
                        | ParentMessage::Prepare { .. }
                        | ParentMessage::Reset
                        | ParentMessage::Cancel
                        | ParentMessage::Shutdown
                ) {
                    return Err(PoolError::protocol(
                        "idle worker expects Start or lifecycle control",
                    ));
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
                .ok_or_else(|| PoolError::protocol("missing lease"))?
                .admit(&frame.header)
                .map_err(PoolError::protocol)?;
            drop(fences);
            if let Some(hook) = hook.as_mut() {
                hook(&frame.message);
            }
            match frame.message {
                ParentMessage::Start(start) => {
                    if self.owner.is_some() {
                        return Err(PoolError::protocol("Start before reset"));
                    }
                    self.owner = Some(start.owner.clone());
                    self.progress(WorkerPhase::Computing)?;
                    let step = self.start(*start)?;
                    self.deliver(step)?;
                }
                ParentMessage::Prepare { owner, request } => {
                    if self.owner.is_some() {
                        return Err(PoolError::protocol("Prepare before reset"));
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
                        .ok_or_else(|| PoolError::protocol("no pending effect"))?;
                    if result.id != request.id {
                        return Err(PoolError::protocol("effect result has wrong request id"));
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
                        ) => return Err(PoolError::protocol("wrong control result")),
                        (_, EffectOutcome::Cancelled) => VmResume::EffectCancelled,
                        (_, EffectOutcome::Value(value)) => {
                            let value: AbilityOutcome = self.decode(&value)?;
                            let wire = Arc::new(crate::projection::Wire::new(
                                self.pipe.try_clone().map_err(PoolError::io)?,
                                self.codec.clone(),
                                self.fences.clone(),
                                self.projection_namespace.clone(),
                            ));
                            let value = wire.rebind_outcome(value);
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
                            return Err(PoolError::protocol(
                                "checkpoint result answered an effect",
                            ));
                        }
                    };
                    let step = self.instance.resume(resume).map_err(PoolError::protocol)?;
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
                        return Err(PoolError::protocol("park answers no parkable request"));
                    }
                    self.progress(WorkerPhase::Computing)?;
                    let step = self
                        .instance
                        .resume(VmResume::Park)
                        .map_err(PoolError::protocol)?;
                    self.deliver(step)?;
                }
                ParentMessage::Cancel => {
                    // Physical cancellation never decides the journaled winner.
                    self.instance.reset();
                    self.pending = None;
                    self.reissue = None;
                    self.send(WorkerMessage::Cancelled)?;
                }
                ParentMessage::Reset => {
                    self.cpu_ceiling = None;
                    self.instance.reset();
                    self.pending = None;
                    self.reissue = None;
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
    fn decode<T: serde::de::DeserializeOwned>(
        &self,
        payload: &EncodedPayload,
    ) -> Result<T, PoolError> {
        if payload.0.len() as u64 > self.bootstrap.effect {
            return Err(PoolError::protocol("effect value too large"));
        }
        self.codec.check_payload(&payload.0)?;
        rmp_serde::from_slice(&payload.0).map_err(PoolError::protocol)
    }
    fn start(&mut self, start: Start) -> Result<VmStep, PoolError> {
        let mut context = RunContext::default();
        for description in &start.contexts {
            if description.kind != "vm_run" {
                return Err(PoolError::protocol("unknown VM context description"));
            }
            self.codec.check_payload(&description.body.0)?;
            context = rmp_serde::from_slice(&description.body.0).map_err(PoolError::protocol)?;
        }
        self.projection_namespace = context.projection_namespace;
        let execution_start = match start.state {
            StartState::Fresh => VmExecutionStart::Session,
            StartState::Snapshot(state) => {
                self.check(&state, VmStateKind::Snapshot)?;
                let snapshot = self
                    .instance
                    .open_snapshot(state.bytes())
                    .map_err(PoolError::protocol)?;
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
                        .map_err(PoolError::protocol)?,
                ))
            }
        };
        let program = match start.program {
            ProgramSource::Source { dialect, text } => {
                if dialect != self.frontend.language_id()
                    || text.len() as u64 > self.bootstrap.source
                {
                    return Err(PoolError::protocol("source dialect or size refused"));
                }
                let ast = self
                    .frontend
                    .parse(&text, Some(&context.environment))
                    .map_err(|refusal| PoolError::protocol(refusal.error))?;
                let linked = self
                    .instance
                    .linked_programs_mut()
                    .get_or_compile_ast(&text, ast, &context.environment)
                    .map_err(PoolError::protocol)?;
                linked.compiled_program().clone()
            }
            ProgramSource::Artifact {
                module_ref,
                entry,
                artifact,
            } => {
                let artifact =
                    ModuleArtifact::from_store_bytes(&artifact).map_err(PoolError::protocol)?;
                if artifact.module_ref().as_str() != module_ref {
                    return Err(PoolError::protocol("artifact identity mismatch"));
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
                lashlang::compile(&artifact, entry, None).map_err(PoolError::protocol)?
            }
        };
        let bound = |v: Option<u64>| -> Result<ExecutionBound<std::num::NonZeroU64>, PoolError> {
            match v {
                None => Ok(ExecutionBound::Unbounded),
                Some(v) => std::num::NonZeroU64::new(v)
                    .map(ExecutionBound::Bounded)
                    .ok_or(PoolError::InvalidConfiguration),
            }
        };
        let depth = std::num::NonZeroU64::new(start.limits.max_frame_depth)
            .ok_or(PoolError::InvalidConfiguration)?;
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
        let wire = Arc::new(crate::projection::Wire::new(
            self.pipe.try_clone().map_err(PoolError::io)?,
            self.codec.clone(),
            self.fences.clone(),
            self.projection_namespace.clone(),
        ));
        for description in context.projected {
            let value = match description.scalar {
                Some(value) => lashlang::ProjectedValue::scalar(description.name.clone(), value),
                None => lashlang::ProjectedValue::custom(
                    format!(
                        "worker-projection/{}/{}/{}",
                        self.projection_namespace, description.key, description.name
                    ),
                    Arc::new(crate::projection::RemoteProjection {
                        wire: wire.clone(),
                        key: description.key,
                        type_name: description.type_name,
                    }),
                ),
            };
            config
                .projected
                .try_insert(description.name, value)
                .map_err(PoolError::protocol)?;
        }
        config.projected = config
            .projected
            .with_resolver(Arc::new(move |value| wire.resolve(value)));
        self.instance
            .start(Arc::new(program), execution_start, config)
            .map_err(PoolError::protocol)
    }
    fn check(&self, state: &OpaqueVmState, kind: VmStateKind) -> Result<(), PoolError> {
        state
            .check(&StateExpectation {
                kind,
                owner: self
                    .owner
                    .as_ref()
                    .ok_or_else(|| PoolError::protocol("missing owner"))?,
                reads: &lashlang::vm_contract_reads(),
                max_bytes: self.bootstrap.state,
            })
            .map_err(PoolError::protocol)
    }
    fn seal(&self, kind: VmStateKind, bytes: Vec<u8>) -> Result<OpaqueVmState, PoolError> {
        if bytes.len() as u64 > self.bootstrap.state {
            return Err(InfrastructureOutcome::PayloadTooLarge {
                limit: self.bootstrap.state,
                size: bytes.len() as u64,
            }
            .into());
        }
        Ok(OpaqueVmState::seal(
            kind,
            self.owner
                .clone()
                .ok_or_else(|| PoolError::protocol("missing owner"))?,
            lashlang::vm_contract_versions(),
            match kind {
                VmStateKind::Snapshot => lashlang::LASHLANG_SNAPSHOT_VERSION,
                VmStateKind::Continuation => lashlang::VM_CONTINUATION_FORMAT_VERSION,
            },
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
                .map_err(PoolError::protocol)?,
        )
    }
    fn respond(&mut self, message: WorkerMessage) -> Result<(), PoolError> {
        // Encode while the serialization deadline is active. Reserve the
        // header after Responding without advancing the real fence yet.
        let mut fence = self
            .fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .outgoing;
        fence.next_header();
        let header = fence.next_header();
        let encode = |message| {
            self.codec
                .encode_worker(&WorkerFrame { header, message })
                .map_err(PoolError::from)
        };
        let bytes = match encode(message) {
            Err(PoolError::Infrastructure(InfrastructureOutcome::PayloadTooLarge {
                limit,
                size,
            })) => encode(WorkerMessage::PayloadTooLarge { limit, size })?,
            result => result?,
        };
        self.progress(WorkerPhase::Responding)?;
        self.fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .outgoing
            .next_header();
        write_frame(
            &mut self.pipe,
            &bytes,
            Instant::now() + Duration::from_secs(30),
        )
    }
    fn deliver(&mut self, step: VmStep) -> Result<(), PoolError> {
        let message = match self.deliver_inner(step) {
            Err(PoolError::Infrastructure(InfrastructureOutcome::PayloadTooLarge {
                limit,
                size,
            })) => WorkerMessage::PayloadTooLarge { limit, size },
            result => result?,
        };
        self.respond(message)
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
        if !observations.is_empty() {
            self.send(WorkerMessage::Observations {
                payload: EncodedPayload(
                    rmp_serde::to_vec_named(observations).map_err(PoolError::protocol)?,
                ),
            })?;
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
                        // The VM wire restores projections by identity. The
                        // request already issued includes their scalar values,
                        // so resume reads that request rather than rebuilding it
                        // from the continuation's unavailable placeholders.
                        match self.reissue.take() {
                            Some(recorded) if recorded.kind == kind => {
                                (recorded.kind, recorded.payload.0)
                            }
                            Some(_) => {
                                return Err(PoolError::protocol("parked request kind changed"));
                            }
                            None => (
                                kind,
                                rmp_serde::to_vec_named(&op).map_err(PoolError::protocol)?,
                            ),
                        }
                    }
                    VmRequest::CancelCheckpoint(n) => (
                        EffectKind::CancelCheckpoint,
                        rmp_serde::to_vec_named(&n).map_err(PoolError::protocol)?,
                    ),
                    VmRequest::Boundary => (EffectKind::ProcessBoundary, Vec::new()),
                    VmRequest::ParkDeclined(error) => (
                        EffectKind::ParkDeclined,
                        rmp_serde::to_vec_named(&error.to_string()).map_err(PoolError::protocol)?,
                    ),
                };
                if payload.len() as u64 > self.bootstrap.effect {
                    return Err(InfrastructureOutcome::PayloadTooLarge {
                        limit: self.bootstrap.effect,
                        size: payload.len() as u64,
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
                let request = EffectRequest {
                    id,
                    kind,
                    payload: EncodedPayload(payload),
                };
                self.pending = Some(request.clone());
                WorkerMessage::EffectRequest(request)
            }
            VmStep::Parked(parked) => {
                let bytes = ParkedRun {
                    vm: EncodedPayload(
                        parked
                            .continuation
                            .to_bytes()
                            .map_err(PoolError::protocol)?,
                    ),
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
            VmStep::Complete(complete) => WorkerMessage::Complete {
                state: self.snapshot()?,
                value: EncodedPayload(
                    rmp_serde::to_vec_named(&complete.outcome).map_err(PoolError::protocol)?,
                ),
            },
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
                            rmp_serde::to_vec_named(&error.failure).map_err(PoolError::protocol)?,
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
    if depth > 64 {
        return Err(PoolError::protocol("terminal value exceeds depth bound"));
    }
    Ok(match value {
        Value::Projected(value) => Value::Projected(lashlang::ProjectedValue::scalar(
            value.name().to_owned(),
            materialize(value.materialize().map_err(PoolError::protocol)?, depth + 1)?,
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
