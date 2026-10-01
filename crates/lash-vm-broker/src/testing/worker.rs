//! The fake worker: a scripted program behind real protocol frames.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use lash_vm_protocol::{
    EffectKind, EffectOutcome, EffectRequest, EffectRequestId, EncodedPayload, ExecutionLease,
    FrameCodec, FrameEpoch, MessageFence, OpaqueVmState, OwnerEpoch, ParentMessage, ProgramSource,
    RunRefusal, SequenceFault, StartState, SupervisorEvidence, VmOwner, VmStateKind, WorkerFrame,
    WorkerMessage, WorkerRefusal,
};
use serde::{Deserialize, Serialize};

use crate::authority::{
    Invocation, OperationRequest, OperationRequestCodec, decode_value, encode_value,
};
use crate::transport::{WorkerRead, WorkerTransport};

use super::pool::PoolShared;

/// The VM contract the fake's state is written under.
pub const FAKE_VM_CONTRACT: lash_vm_protocol::VmContract = lash_vm_protocol::VmContract {
    bytecode: 1,
    continuation: FAKE_STATE_FORMAT,
    snapshot: FAKE_STATE_FORMAT,
    accounting: 1,
    heap: 1,
    abi: 1,
};

/// The format version of the fake's state, continuation and snapshot alike.
pub const FAKE_STATE_FORMAT: u32 = 1;

/// The dialect a scripted program's source is spelled in.
const DIALECT: &str = "lash-vm-broker-scripted";

/// One step of a scripted program.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// A resource operation; its value is the step's result.
    Invoke(Invocation),
    /// An aggregate of resource operations; its value is the step's result.
    Aggregate(Vec<Invocation>),
    /// Awaits the handle the `from` result named under its `handle` key.
    AwaitHandleOf { from: usize },
    /// A durable sleep.
    Sleep(u64),
    /// A cancel checkpoint: a cancelled answer ends the run cancelled.
    Checkpoint(u64),
    /// Pure computation: where a mid-compute kill lands.
    Compute,
    /// Computation that never ends and ignores a cooperative cancel.
    Hang,
    /// Parks the run here, as a segment boundary does.
    Boundary,
    SetGlobal {
        name: String,
        value: serde_json::Value,
    },
    /// Reads a session global into the results: `"undefined"` when unset.
    ReadGlobal { name: String },
    /// Sends a request of any kind and payload, as a compromised VM could;
    /// its answer is the step's result.
    Raw { kind: EffectKind, payload: Vec<u8> },
}

/// A program the fake worker runs: its steps, in order. The run completes
/// with the list of its results.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScriptedProgram {
    pub steps: Vec<Step>,
}

impl ScriptedProgram {
    pub fn new(steps: Vec<Step>) -> Self {
        Self { steps }
    }

    /// The program as the source a `Start` carries.
    pub fn source(&self) -> ProgramSource {
        ProgramSource::Source {
            dialect: DIALECT.to_string(),
            text: serde_json::to_string(self).unwrap_or_default(),
        }
    }

    fn from_source(source: &ProgramSource) -> Option<Self> {
        match source {
            ProgramSource::Source { dialect, text } if dialect == DIALECT => {
                serde_json::from_str(text).ok()
            }
            _ => None,
        }
    }
}

/// Where a planned fault strikes one checkout's worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The worker dies as its `Start` arrives: nothing of the run happens.
    DieBeforeStart,
    /// The worker refuses the run as its `Start` arrives, as one that cannot
    /// read an input of the run does: nothing of the run happens, and every
    /// attempt is refused the same way.
    RefuseRun,
    /// The worker answers its `Start` with a breach of the exchange, as one
    /// that met a message out of order does.
    BreachAtStart,
    /// The worker dies at the program's first `Compute` step.
    DieMidCompute,
    /// The worker sends its request number `n` (counting from zero), then
    /// dies before any answer.
    DieAfterRequest(usize),
    /// The worker dies as the answer to its request number `n` arrives: the
    /// parent performed and journaled it, and delivery fails.
    DieBeforeDelivery(usize),
    /// The worker's next state-carrying frame is cut off halfway, and the
    /// worker dies.
    DieMidSerialization,
    /// The worker sends its `Complete` whole, then dies.
    DieAfterComplete,
    /// Request number `n` travels under an earlier lease.
    StaleLease(usize),
    /// Request number `n` travels under an earlier frame epoch.
    StaleFrameEpoch(usize),
    /// Request number `n`'s frame is sent twice, byte for byte.
    ReplayedFrame(usize),
    /// Request number `n` is sent again, under the next sequence, with the
    /// same request id.
    RepeatedRequestId(usize),
    /// The worker cannot capture its run where it stands: it answers every
    /// park by asking `ParkDeclined`, and once that is acknowledged issues
    /// the request it stands on again.
    DeclinePark,
}

/// The fake VM's state: where the program stands, what it has, and the
/// session's globals.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct FakeState {
    pc: usize,
    results: Vec<serde_json::Value>,
    globals: BTreeMap<String, serde_json::Value>,
}

/// One checkout's worker and the transport the broker holds it by.
pub(super) struct FakeWorker {
    codec: FrameCodec,
    lease: ExecutionLease,
    owner: Option<VmOwner>,
    fault: Option<Fault>,
    incoming: MessageFence,
    outgoing: MessageFence,
    out: VecDeque<WorkerRead>,
    dead: Option<SupervisorEvidence>,
    program: Option<ScriptedProgram>,
    state: FakeState,
    /// The request the worker waits on, and the step that issued it.
    pending: Option<EffectRequestId>,
    /// The pending request is a declined park, not a step of the program.
    declining: bool,
    next_request: u64,
    /// How many effect requests the worker sent.
    requests_sent: usize,
    interrupted: bool,
    shared: Arc<PoolShared>,
}

impl FakeWorker {
    pub(super) fn new(
        codec: FrameCodec,
        lease: ExecutionLease,
        owner_epoch: OwnerEpoch,
        frame_epoch: FrameEpoch,
        fault: Option<Fault>,
        shared: Arc<PoolShared>,
    ) -> Self {
        let mut worker = Self {
            incoming: MessageFence::new(lease, owner_epoch, frame_epoch),
            outgoing: MessageFence::new(lease, owner_epoch, frame_epoch),
            codec,
            lease,
            owner: None,
            fault,
            out: VecDeque::new(),
            dead: None,
            program: None,
            state: FakeState::default(),
            pending: None,
            declining: false,
            next_request: 0,
            requests_sent: 0,
            interrupted: false,
            shared,
        };
        worker.emit(WorkerMessage::Ready {
            protocol_version: lash_vm_protocol::WORKER_PROTOCOL_VERSION,
            crate_version: env!("CARGO_PKG_VERSION").into(),
        });
        worker
    }

    fn die(&mut self, evidence: SupervisorEvidence) {
        if self.dead.is_none() {
            self.dead = Some(evidence);
            self.shared.record_death();
        }
    }

    fn crash() -> SupervisorEvidence {
        SupervisorEvidence::Exited { code: 101 }
    }

    fn emit(&mut self, message: WorkerMessage) {
        let frame = WorkerFrame {
            header: self.outgoing.next_header(),
            message,
        };
        self.emit_frame(frame);
    }

    fn emit_frame(&mut self, frame: WorkerFrame) {
        match self.codec.encode_worker(&frame) {
            Ok(bytes) => self.out.push_back(WorkerRead::Bytes(bytes)),
            Err(_) => self.die(Self::crash()),
        }
    }

    fn handle(&mut self, message: ParentMessage) {
        match message {
            ParentMessage::Prepare { .. } => {
                self.die(SupervisorEvidence::Exited { code: 1 });
            }
            ParentMessage::Start(start) => {
                if self.fault == Some(Fault::DieBeforeStart) {
                    self.die(Self::crash());
                    return;
                }
                let refusal = match self.fault {
                    Some(Fault::RefuseRun) => Some(WorkerRefusal::Run(RunRefusal::UnknownContext)),
                    Some(Fault::BreachAtStart) => Some(WorkerRefusal::Breach(
                        SequenceFault::StartBeforeReset.into(),
                    )),
                    _ => None,
                };
                if let Some(refusal) = refusal {
                    self.emit(WorkerMessage::Refused { refusal });
                    self.die(SupervisorEvidence::Exited { code: 1 });
                    return;
                }
                self.shared.record_start();
                self.owner = Some(start.owner.clone());
                self.program = ScriptedProgram::from_source(&start.program);
                self.state = match start.state {
                    StartState::Fresh => FakeState::default(),
                    StartState::Snapshot(state) => FakeState {
                        globals: open(&state).map(|state| state.globals).unwrap_or_default(),
                        ..FakeState::default()
                    },
                    StartState::Continuation(state) => open(&state).unwrap_or_default(),
                };
                self.advance();
            }
            ParentMessage::EffectResponse(result) => {
                if self.pending != Some(result.id) {
                    self.die(Self::crash());
                    return;
                }
                self.pending = None;
                if std::mem::take(&mut self.declining) {
                    if result.outcome != EffectOutcome::Unit {
                        self.die(Self::crash());
                        return;
                    }
                    // The run still stands on the step whose request it
                    // was asked to park on, and issues it again.
                    self.advance();
                    return;
                }
                match result.outcome {
                    EffectOutcome::Cancelled => {
                        self.emit(WorkerMessage::Cancelled);
                        return;
                    }
                    EffectOutcome::Value(value) => self
                        .state
                        .results
                        .push(decode_value(&value).unwrap_or(serde_json::Value::Null)),
                    EffectOutcome::Failed(error) => self.state.results.push(serde_json::json!({
                        "failed": decode_value(&error).unwrap_or(serde_json::Value::Null),
                    })),
                    EffectOutcome::Unit | EffectOutcome::HandedOver => {
                        self.state.results.push(serde_json::Value::Null);
                    }
                    EffectOutcome::Checkpoint { cancelled } => {
                        if cancelled {
                            self.emit(WorkerMessage::Cancelled);
                            return;
                        }
                    }
                }
                self.state.pc += 1;
                self.advance();
            }
            ParentMessage::Park => {
                if self.pending.is_none() {
                    self.die(Self::crash());
                    return;
                }
                self.pending = None;
                if self.fault == Some(Fault::DeclinePark) {
                    self.declining = true;
                    // The reason, encoded as the worker entry encodes it.
                    self.request(
                        EffectKind::ParkDeclined,
                        EncodedPayload(
                            rmp_serde::to_vec_named("the fake worker declines its park")
                                .unwrap_or_default(),
                        ),
                    );
                    return;
                }
                // The run stands on the step that issued the request, and
                // issues it again when it resumes.
                self.emit_state(
                    |state| WorkerMessage::Suspended { state },
                    VmStateKind::Continuation,
                );
            }
            ParentMessage::Cancel => self.interrupted = true,
            ParentMessage::Reset => {
                self.state = FakeState::default();
                self.program = None;
                self.emit(WorkerMessage::ResetDone { cpu_nanos: 0 });
            }
            ParentMessage::Shutdown => self.die(SupervisorEvidence::Exited { code: 0 }),
        }
    }

    /// Runs the program until it needs its parent or ends.
    fn advance(&mut self) {
        loop {
            if self.dead.is_some() {
                return;
            }
            let Some(step) = self
                .program
                .as_ref()
                .and_then(|program| program.steps.get(self.state.pc))
                .cloned()
            else {
                let value = encode_value(&serde_json::Value::Array(self.state.results.clone()));
                let fault = self.fault.clone();
                self.emit_state(
                    |state| WorkerMessage::Complete { state, value },
                    VmStateKind::Snapshot,
                );
                if fault == Some(Fault::DieAfterComplete) {
                    self.die(Self::crash());
                }
                return;
            };
            if self.interrupted && !matches!(step, Step::Hang) {
                self.emit(WorkerMessage::Cancelled);
                return;
            }
            match step {
                Step::Invoke(invocation) => {
                    return self
                        .request(EffectKind::ResourceOperation, invocation.request().encode());
                }
                Step::Aggregate(members) => {
                    return self.request(
                        EffectKind::ResourceOperationBatch,
                        OperationRequest::ResourceOperationBatch(
                            lashlang::ResourceOperationBatch {
                                leaves: members
                                    .into_iter()
                                    .map(|member| {
                                        let OperationRequest::ResourceOperation(op) =
                                            member.request()
                                        else {
                                            unreachable!()
                                        };
                                        lashlang::ResourceOperationBatchLeaf::Operation(*op)
                                    })
                                    .collect(),
                                consumer: lashlang::AggregateConsumer::All,
                                settled_value_after: None,
                            },
                        )
                        .encode(),
                    );
                }
                Step::AwaitHandleOf { from } => {
                    let handle = self
                        .state
                        .results
                        .get(from)
                        .and_then(|result| result.get("handle"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    return self.request(
                        EffectKind::Await,
                        OperationRequest::Await(lashlang::Value::String(handle.into())).encode(),
                    );
                }
                Step::Sleep(millis) => {
                    return self.request(
                        EffectKind::Sleep,
                        OperationRequest::Sleep(lashlang::Sleep {
                            value: lashlang::Value::Number(millis as f64),
                            kind: lashlang::SleepKind::For,
                            call_site: None,
                        })
                        .encode(),
                    );
                }
                Step::Checkpoint(checkpoint) => {
                    return self.request(
                        EffectKind::CancelCheckpoint,
                        EncodedPayload(rmp_serde::to_vec(&checkpoint).unwrap_or_default()),
                    );
                }
                Step::Raw { kind, payload } => {
                    return self.request(kind, EncodedPayload(payload));
                }
                Step::Compute => {
                    if self.fault == Some(Fault::DieMidCompute) {
                        self.die(Self::crash());
                        return;
                    }
                }
                Step::Hang => {
                    self.shared.record_hang();
                    return;
                }
                Step::Boundary => {
                    self.state.pc += 1;
                    self.emit_state(
                        |state| WorkerMessage::Suspended { state },
                        VmStateKind::Continuation,
                    );
                    return;
                }
                Step::SetGlobal { name, value } => {
                    self.state.globals.insert(name, value);
                }
                Step::ReadGlobal { name } => {
                    let value = self
                        .state
                        .globals
                        .get(&name)
                        .cloned()
                        .unwrap_or_else(|| serde_json::Value::String("undefined".into()));
                    self.state.results.push(value);
                }
            }
            self.state.pc += 1;
        }
    }

    fn request(&mut self, kind: EffectKind, payload: EncodedPayload) {
        let id = EffectRequestId(self.next_request);
        self.next_request += 1;
        self.pending = Some(id);
        let number = self.requests_sent;
        self.requests_sent += 1;
        let request = WorkerMessage::EffectRequest(EffectRequest { id, kind, payload });
        let mut frame = WorkerFrame {
            header: self.outgoing.next_header(),
            message: request.clone(),
        };
        match &self.fault {
            Some(Fault::StaleLease(n)) if *n == number => {
                frame.header.lease = ExecutionLease(self.lease.0.wrapping_sub(1));
            }
            Some(Fault::StaleFrameEpoch(n)) if *n == number => {
                frame.header.frame_epoch = FrameEpoch(frame.header.frame_epoch.0.wrapping_sub(1));
            }
            _ => {}
        }
        self.emit_frame(frame.clone());
        match &self.fault {
            Some(Fault::ReplayedFrame(n)) if *n == number => self.emit_frame(frame),
            Some(Fault::RepeatedRequestId(n)) if *n == number => self.emit(request),
            Some(Fault::DieAfterRequest(n)) if *n == number => self.die(Self::crash()),
            _ => {}
        }
    }

    fn emit_state(
        &mut self,
        message: impl FnOnce(OpaqueVmState) -> WorkerMessage,
        kind: VmStateKind,
    ) {
        let owner = self.owner.clone().unwrap_or_else(|| VmOwner::new(""));
        let state = OpaqueVmState::seal(
            kind,
            owner,
            FAKE_VM_CONTRACT,
            serde_json::to_vec(&self.state).unwrap_or_default(),
        );
        let frame = WorkerFrame {
            header: self.outgoing.next_header(),
            message: message(state),
        };
        if self.fault == Some(Fault::DieMidSerialization) {
            if let Ok(bytes) = self.codec.encode_worker(&frame) {
                self.out
                    .push_back(WorkerRead::Bytes(bytes[..bytes.len() / 2].to_vec()));
            }
            self.die(Self::crash());
            return;
        }
        self.emit_frame(frame);
    }
}

fn open(state: &OpaqueVmState) -> Option<FakeState> {
    serde_json::from_slice(state.bytes()).ok()
}

#[async_trait::async_trait]
impl WorkerTransport for FakeWorker {
    async fn send(&mut self, frame: Vec<u8>) -> Result<(), SupervisorEvidence> {
        if let Some(evidence) = self.dead {
            return Err(evidence);
        }
        let Ok(frame) = self.codec.decode_parent(&frame) else {
            self.die(Self::crash());
            return Err(Self::crash());
        };
        if self.incoming.admit(&frame.header).is_err() {
            self.die(Self::crash());
            return Err(Self::crash());
        }
        if let (ParentMessage::EffectResponse(_), Some(Fault::DieBeforeDelivery(n))) =
            (&frame.message, &self.fault)
            && self.requests_sent == n + 1
        {
            self.die(Self::crash());
            return Err(Self::crash());
        }
        self.handle(frame.message);
        Ok(())
    }

    async fn recv(&mut self) -> WorkerRead {
        if let Some(read) = self.out.pop_front() {
            return read;
        }
        match self.dead {
            Some(evidence) => WorkerRead::Ended(evidence),
            None => std::future::pending().await,
        }
    }

    async fn kill(&mut self) -> SupervisorEvidence {
        self.out.clear();
        self.die(SupervisorEvidence::Signalled { signal: 9 });
        self.dead
            .unwrap_or(SupervisorEvidence::Signalled { signal: 9 })
    }
}
