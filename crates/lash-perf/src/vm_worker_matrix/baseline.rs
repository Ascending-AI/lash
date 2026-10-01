//! The retained in-parent benchmark reference, never a production execution mode.
use super::workload::{Case, Host, environment};
use anyhow::{Result, bail};
use lashlang::{
    ExecutionBounds, ExecutionMode, VmExecutionStart, VmInstance, VmRequest, VmResume, VmRunConfig,
    VmStep,
};
use std::sync::Arc;

fn run_owned(case: &Case) -> Result<()> {
    let mut instance = VmInstance::pristine();
    let mut host = Host::default();
    let mut outcome = None;
    let mode = if case.resumed {
        ExecutionMode::Process
    } else {
        ExecutionMode::Foreground
    };
    for source in &case.cells {
        let program = lash_typescript::parse_cell(source, &environment())
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let spans = program.spans.clone();
        let artifact = lashlang::ModuleArtifact::from_program(program)?;
        let compiled = Arc::new(lashlang::compile(
            &artifact,
            lashlang::Entry::Main,
            Some(&spans),
        )?);
        let config = VmRunConfig::new(mode, ExecutionBounds::unbounded());
        let mut step =
            instance.start(compiled.clone(), VmExecutionStart::Session, config.clone())?;
        loop {
            step = match step {
                VmStep::Suspended(suspended) => {
                    let answer = match suspended.request {
                        VmRequest::Effect(op) => VmResume::Effect(host.perform(op)),
                        VmRequest::CancelCheckpoint(_) => {
                            VmResume::CancelCheckpoint { cancelled: false }
                        }
                        VmRequest::Boundary => VmResume::Park,
                        VmRequest::ParkDeclined(_) => bail!("baseline declined park"),
                    };
                    instance.resume(answer)?
                }
                VmStep::Parked(parked) => {
                    let bytes = parked.continuation.to_bytes()?;
                    instance = VmInstance::pristine();
                    let continuation = instance.open_continuation(&bytes)?;
                    instance.start(
                        compiled.clone(),
                        VmExecutionStart::Continuation(Box::new(continuation)),
                        config.clone(),
                    )?
                }
                VmStep::Complete(complete) => {
                    outcome = Some(complete.outcome);
                    break;
                }
                VmStep::GuestError(error) => {
                    if !case.error {
                        bail!("unexpected guest error: {:?}", error.failure);
                    }
                    break;
                }
            }
        }
    }
    host.check(case, outcome.as_ref())
}

/// The f0 State/execute boundary. Its executor is created outside the timer.
pub struct Reference(tokio::runtime::Runtime);
impl Reference {
    pub fn new() -> Result<Self> {
        Ok(Self(tokio::runtime::Builder::new_current_thread().build()?))
    }
    pub fn run(&self, case: &Case) -> Result<()> {
        if case.resumed {
            return run_owned(case);
        }
        let mut state = lashlang::State::new();
        let host = ImmediateHost(std::sync::Mutex::new(Host::default()));
        let mut outcome = None;
        for source in &case.cells {
            let program = if case.effects == 0 {
                let globals = state.binding_names().map(str::to_string).collect();
                lash_typescript::parse_with_globals(source, &globals)
            } else {
                lash_typescript::parse_cell(source, &environment())
            }
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            let spans = program.spans.clone();
            let artifact = lashlang::ModuleArtifact::from_program(program)?;
            let compiled = lashlang::compile(&artifact, lashlang::Entry::Main, Some(&spans))?;
            match self
                .0
                .block_on(lashlang::execute(&compiled, &mut state, &host))
            {
                Ok(value) => outcome = Some(value),
                Err(error) if case.error => {
                    std::hint::black_box(error);
                }
                Err(error) => bail!("unexpected baseline error: {error:?}"),
            }
        }
        host.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .check(case, outcome.as_ref())
    }
}
struct ImmediateHost(std::sync::Mutex<Host>);
impl lashlang::ExecutionHost for ImmediateHost {
    async fn perform(
        &self,
        op: lashlang::AbilityOp,
    ) -> Result<lashlang::AbilityOutcome, lashlang::ExecutionHostError> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .perform(op)
    }
}

/// The production payload and frame codecs over a socket pair in this process.
/// The peer lives for the whole run; thread creation is outside every sample.
pub struct CodecSocket {
    pipe: std::os::unix::net::UnixStream,
    inbound: lash_vm_client::ipc::FrameSource,
    codec: lash_vm_protocol::FrameCodec,
    peer: Option<std::thread::JoinHandle<Result<()>>>,
}
impl CodecSocket {
    pub fn new() -> Result<Self> {
        use lash_vm_protocol::*;
        let (pipe, mut peer_pipe) = std::os::unix::net::UnixStream::pair()?;
        let limits = ProtocolBounds::standard().decode;
        let peer = std::thread::spawn(move || -> Result<()> {
            let codec = FrameCodec::new(limits);
            let mut inbound = lash_vm_client::ipc::FrameSource::default();
            let mut host = super::workload::Host::default();
            loop {
                let bytes = match inbound.read_frame(
                    &mut peer_pipe,
                    &codec,
                    std::time::Instant::now() + std::time::Duration::from_secs(86_400),
                ) {
                    Ok(bytes) => bytes,
                    Err(lash_vm_client::PoolError::Infrastructure(
                        InfrastructureOutcome::WorkerCrashed { .. },
                    )) => return Ok(()),
                    Err(error) => return Err(error.into()),
                };
                let WorkerMessage::EffectRequest(request) = codec.decode_worker(&bytes)?.message
                else {
                    anyhow::bail!("baseline expected value request");
                };
                let op: lashlang::AbilityOp = rmp_serde::from_slice(&request.payload.0)?;
                let answer = host.perform(op).map_err(|e| anyhow::anyhow!("{e}"))?;
                let response = ParentFrame {
                    header: MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0))
                        .next_header(),
                    message: ParentMessage::EffectResponse(EffectResponse {
                        id: request.id,
                        outcome: EffectOutcome::Value(EncodedPayload(rmp_serde::to_vec_named(
                            &answer,
                        )?)),
                    }),
                };
                let bytes = codec.encode_parent(&response)?;
                lash_vm_client::ipc::write_frame(
                    &mut peer_pipe,
                    &bytes,
                    std::time::Instant::now() + std::time::Duration::from_secs(30),
                )?;
            }
        });
        Ok(Self {
            pipe,
            inbound: lash_vm_client::ipc::FrameSource::default(),
            codec: FrameCodec::new(limits),
            peer: Some(peer),
        })
    }

    pub fn measure(
        &mut self,
        request: &lash_vm_protocol::EffectRequest,
        answer: &lashlang::AbilityOutcome,
    ) -> Result<i64> {
        use lash_vm_protocol::*;
        // Decode the fixture outside timing, then encode that identical value
        // inside it. Both directions also use the production bounded frame codec.
        let op: lashlang::AbilityOp = rmp_serde::from_slice(&request.payload.0)?;
        let measured = std::time::Instant::now();
        let frame = WorkerFrame {
            header: MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0))
                .next_header(),
            message: WorkerMessage::EffectRequest(EffectRequest {
                id: request.id,
                kind: request.kind,
                payload: EncodedPayload(rmp_serde::to_vec_named(&op)?),
            }),
        };
        let bytes = self.codec.encode_worker(&frame)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        lash_vm_client::ipc::write_frame(&mut self.pipe, &bytes, deadline)?;
        let bytes = self
            .inbound
            .read_frame(&mut self.pipe, &self.codec, deadline)?;
        let ParentMessage::EffectResponse(response) = self.codec.decode_parent(&bytes)?.message
        else {
            anyhow::bail!("baseline expected value response");
        };
        let EffectOutcome::Value(value) = response.outcome else {
            anyhow::bail!("baseline expected encoded value");
        };
        self.codec.check_payload(&value.0)?;
        let decoded: lashlang::AbilityOutcome = rmp_serde::from_slice(&value.0)?;
        let elapsed = super::nanos(measured) as i64;
        anyhow::ensure!(
            response.id == request.id
                && matches!((&decoded, answer), (lashlang::AbilityOutcome::Value(actual), lashlang::AbilityOutcome::Value(expected)) if actual == expected),
            "baseline changed the value"
        );
        Ok(elapsed)
    }
}
impl Drop for CodecSocket {
    fn drop(&mut self) {
        let _ = self.pipe.shutdown(std::net::Shutdown::Both);
        if let Some(peer) = self.peer.take() {
            let _ = peer.join();
        }
    }
}
