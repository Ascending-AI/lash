use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::PoolError;
use crate::process::{Bootstrap, read_frame, write_frame};
use lash_vm_protocol::*;
use lashlang::{
    AbilityOp, AbilityOutcome, Entry, ExecutionBound, ExecutionBounds, ExecutionMode,
    LashlangHostEnvironment, ModuleArtifact, RuntimeError, State, VmExecutionStart, VmInstance,
    VmRequest, VmResume, VmRunConfig, VmStep,
};

/// Explicit compiler/VM descriptions, containing no host handles or grants.
/// Encoded as JSON in a `ContextDescription` with kind `vm_run`.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunContext {
    pub environment: LashlangHostEnvironment,
    pub mode: ExecutionMode,
}

pub(crate) struct Server {
    pipe: UnixStream,
    codec: FrameCodec,
    bootstrap: Bootstrap,
    instance: VmInstance,
    incoming: Option<MessageFence>,
    outgoing: MessageFence,
    owner: Option<VmOwner>,
    pending: Option<(EffectRequestId, EffectKind)>,
    next_effect: u64,
}

impl Server {
    pub(crate) fn new(
        pipe: UnixStream,
        codec: FrameCodec,
        bootstrap: Bootstrap,
        build: BuildIdentity,
    ) -> Result<Self, PoolError> {
        let mut server = Self {
            pipe,
            codec,
            bootstrap,
            instance: VmInstance::pristine(),
            incoming: None,
            outgoing: MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0)),
            owner: None,
            pending: None,
            next_effect: 0,
        };
        server.send(WorkerMessage::Ready { build })?;
        Ok(server)
    }

    fn send(&mut self, message: WorkerMessage) -> Result<(), PoolError> {
        let bytes = self
            .codec
            .encode_worker(&WorkerFrame {
                header: self.outgoing.next_header(),
                message,
            })
            .map_err(PoolError::from)?;
        write_frame(
            &mut self.pipe,
            &bytes,
            Instant::now() + Duration::from_secs(30),
        )
    }
    fn progress(&mut self, phase: WorkerPhase) -> Result<(), PoolError> {
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
            if self.incoming.is_none() {
                if !matches!(
                    frame.message,
                    ParentMessage::Start(_)
                        | ParentMessage::Reset
                        | ParentMessage::Cancel
                        | ParentMessage::Shutdown
                ) {
                    return Err(PoolError::protocol(
                        "idle worker expects Start or lifecycle control",
                    ));
                }
                self.incoming = Some(MessageFence::new(
                    frame.header.lease,
                    frame.header.owner_epoch,
                    frame.header.frame_epoch,
                ));
                self.outgoing = MessageFence::new(
                    frame.header.lease,
                    frame.header.owner_epoch,
                    frame.header.frame_epoch,
                );
            }
            self.incoming
                .as_mut()
                .ok_or_else(|| PoolError::protocol("missing lease"))?
                .admit(&frame.header)
                .map_err(PoolError::protocol)?;
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
                    match self.start(*start) {
                        Ok(step) => self.deliver(step)?,
                        Err(error) => {
                            self.progress(WorkerPhase::Serializing)?;
                            self.respond(WorkerMessage::GuestError {
                                state: None,
                                error: EncodedPayload(
                                    serde_json::to_vec(&error.to_string())
                                        .map_err(PoolError::protocol)?,
                                ),
                            })?;
                        }
                    }
                }
                ParentMessage::EffectResponse(result) => {
                    let (id, kind) = self
                        .pending
                        .take()
                        .ok_or_else(|| PoolError::protocol("no pending effect"))?;
                    if result.id != id {
                        return Err(PoolError::protocol("effect result has wrong request id"));
                    }
                    self.progress(WorkerPhase::Computing)?;
                    let resume = match (kind, result.outcome) {
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
                        (_, EffectOutcome::Value(value)) => {
                            VmResume::Effect(Ok(self.decode(&value)?))
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
                    if !matches!(self.pending.take(), Some((_, EffectKind::ProcessBoundary))) {
                        return Err(PoolError::PendingEffectParkingRequired);
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
                    self.send(WorkerMessage::Cancelled)?;
                }
                ParentMessage::Reset => {
                    self.instance.reset();
                    self.pending = None;
                    self.owner = None;
                    self.next_effect = 0;
                    self.send(WorkerMessage::ResetDone)?;
                    self.incoming = None;
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
        serde_json::from_slice(&payload.0).map_err(PoolError::protocol)
    }
    fn start(&mut self, start: Start) -> Result<VmStep, PoolError> {
        let mut context = RunContext::default();
        for description in &start.contexts {
            if description.kind != "vm_run" {
                return Err(PoolError::protocol("unknown VM context description"));
            }
            context = serde_json::from_slice(&description.body.0).map_err(PoolError::protocol)?;
        }
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
                VmExecutionStart::Continuation(Box::new(
                    self.instance
                        .open_continuation(state.bytes())
                        .map_err(PoolError::protocol)?,
                ))
            }
        };
        let program = match start.program {
            ProgramSource::Source { dialect, text } => {
                if dialect != "typescript" || text.len() as u64 > self.bootstrap.source {
                    return Err(PoolError::protocol("source dialect or size refused"));
                }
                let ast = lash_typescript::parse_cell(&text, &context.environment)
                    .map_err(PoolError::protocol)?;
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
                let entry = if entry == "main" {
                    Entry::Main
                } else {
                    Entry::Process(
                        artifact
                            .process_ref(&entry)
                            .ok_or_else(|| PoolError::protocol("missing artifact entry"))?,
                    )
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
        self.instance
            .start(
                Arc::new(program),
                execution_start,
                VmRunConfig::new(
                    context.mode,
                    ExecutionBounds::new(
                        bound(start.limits.instruction_budget)?,
                        bound(start.limits.memory_limit_bytes)?,
                    )
                    .with_max_frame_depth(depth),
                ),
            )
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
                vm_contract: &lashlang::vm_contract_identity(),
                format_version: match kind {
                    VmStateKind::Snapshot => lashlang::LASHLANG_SNAPSHOT_VERSION,
                    VmStateKind::Continuation => lashlang::VM_CONTINUATION_FORMAT_VERSION,
                },
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
            lashlang::vm_contract_identity(),
            match kind {
                VmStateKind::Snapshot => lashlang::LASHLANG_SNAPSHOT_VERSION,
                VmStateKind::Continuation => lashlang::VM_CONTINUATION_FORMAT_VERSION,
            },
            bytes,
        ))
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
        let mut fence = self.outgoing;
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
        self.outgoing.next_header();
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
    fn deliver_inner(&mut self, step: VmStep) -> Result<WorkerMessage, PoolError> {
        self.progress(WorkerPhase::Serializing)?;
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
                        (kind, serde_json::to_vec(&op).map_err(PoolError::protocol)?)
                    }
                    VmRequest::CancelCheckpoint(n) => (
                        EffectKind::CancelCheckpoint,
                        serde_json::to_vec(&n).map_err(PoolError::protocol)?,
                    ),
                    VmRequest::Boundary => (EffectKind::ProcessBoundary, Vec::new()),
                    VmRequest::ParkDeclined(error) => (
                        EffectKind::ParkDeclined,
                        serde_json::to_vec(&error.to_string()).map_err(PoolError::protocol)?,
                    ),
                };
                if payload.len() as u64 > self.bootstrap.effect {
                    return Err(InfrastructureOutcome::PayloadTooLarge {
                        limit: self.bootstrap.effect,
                        size: payload.len() as u64,
                    }
                    .into());
                }
                let id = EffectRequestId(self.next_effect);
                self.next_effect += 1;
                self.pending = Some((id, kind));
                WorkerMessage::EffectRequest(EffectRequest {
                    id,
                    kind,
                    payload: EncodedPayload(payload),
                })
            }
            VmStep::Parked(parked) => WorkerMessage::Suspended {
                state: self.seal(
                    VmStateKind::Continuation,
                    parked
                        .continuation
                        .to_bytes()
                        .map_err(PoolError::protocol)?,
                )?,
            },
            VmStep::Complete(complete) => WorkerMessage::Complete {
                state: self.snapshot()?,
                value: EncodedPayload(
                    serde_json::to_vec(&complete.outcome).map_err(PoolError::protocol)?,
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
                            serde_json::to_vec(&error.failure.error)
                                .map_err(PoolError::protocol)?,
                        ),
                    },
                }
            }
        };
        Ok(message)
    }
}

fn cpu_nanos() -> Result<u64, PoolError> {
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
