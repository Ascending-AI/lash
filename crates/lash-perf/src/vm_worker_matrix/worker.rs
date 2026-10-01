use super::workload::{Case, Host, environment};
use anyhow::{Result, bail};
use lash_vm_client::{
    ExecutionBudget, ParkOutcome, RunContext, WorkerPool, WorkerPoolRuntimeOps as _,
};
use lash_vm_protocol::*;
use lashlang::{ExecutionMode, ExecutionOutcome};
use std::time::Instant;

#[derive(Default)]
pub struct Observation {
    pub queue_ns: u64,
    pub exchange_ns: Vec<u64>,
    pub exchange_phases: Vec<super::metrics::Phases>,
    pub value_fixtures: Vec<(EffectRequest, lashlang::AbilityOutcome)>,
    pub resets_ns: Vec<u64>,
    pub state_bytes: u64,
    pub pid: u32,
    pub worker_rss_kib: Option<u64>,
    pub worker_peak_kib: Option<u64>,
    pub checkouts: usize,
}
pub fn start(
    case: &Case,
    source: &str,
    state: StartState,
    pool: &WorkerPool,
    owner: &VmOwner,
) -> Result<Start> {
    Ok(Start {
        owner: owner.clone(),
        program: ProgramSource::Source {
            dialect: "typescript".into(),
            text: source.into(),
        },
        contexts: vec![ContextDescription {
            kind: "vm_run".into(),
            name: "benchmark".into(),
            body: EncodedPayload(rmp_serde::to_vec_named(&RunContext {
                environment: environment(),
                mode: if case.resumed {
                    ExecutionMode::Process
                } else {
                    ExecutionMode::Foreground
                },
                ..RunContext::default()
            })?),
        }],
        state,
        limits: pool.config().vm_limits,
    })
}
pub fn run(case: &Case, pool: &WorkerPool, owner: &VmOwner) -> Result<Observation> {
    run_observed(case, pool, owner, false)
}
pub fn run_observed(
    case: &Case,
    pool: &WorkerPool,
    owner: &VmOwner,
    sample_memory: bool,
) -> Result<Observation> {
    let mut observation = Observation::default();
    let mut state = StartState::Fresh;
    let mut host = Host::default();
    let mut outcome: Option<ExecutionOutcome> = None;
    for source in &case.cells {
        loop {
            let start = start(case, source, state.clone(), pool, owner)?;
            let bytes = FrameCodec::new(pool.config().protocol.decode)
                .encode_parent(&ParentFrame {
                    // Reserve the largest lease encoding, as the broker does. A
                    // zero lease under-reserves after enough clean checkouts.
                    header: MessageFence::new(
                        ExecutionLease(u64::MAX),
                        OwnerEpoch(0),
                        FrameEpoch(0),
                    )
                    .next_header(),
                    message: ParentMessage::Start(Box::new(start.clone())),
                })?
                .len();
            let queued = Instant::now();
            let mut worker = pool.checkout(
                bytes,
                OwnerEpoch(0),
                FrameEpoch(0),
                ExecutionBudget::default(),
            )?;
            observation.queue_ns += super::nanos(queued);
            observation.checkouts += 1;
            observation.pid = worker
                .pid()
                .ok_or_else(|| anyhow::anyhow!("worker has no pid"))?;
            let mut message = worker.start_measured(start)?;
            let mut parked = false;
            loop {
                message = match message {
                    WorkerMessage::EffectRequest(request) => {
                        if request.kind == EffectKind::ProcessBoundary && case.resumed {
                            let ParkOutcome::Parked(continuation) = worker.park()? else {
                                bail!("worker declined process boundary park");
                            };
                            observation.state_bytes = observation
                                .state_bytes
                                .max(continuation.bytes().len() as u64);
                            state = StartState::Continuation(continuation);
                            parked = true;
                            break;
                        }
                        let timed = matches!(
                            request.kind,
                            EffectKind::ResourceOperation | EffectKind::ResourceOperationBatch
                        );
                        let initial = worker.exchange_timing()?;
                        anyhow::ensure!(
                            initial.response_started_ns > 0,
                            "missing request serialization start"
                        );
                        let mut parent_decode = 0;
                        let mut parent_encode = 0;
                        let mut host_ns = 0;
                        let mut value_answer = None;
                        let result = match request.kind {
                            EffectKind::CancelCheckpoint => {
                                EffectOutcome::Checkpoint { cancelled: false }
                            }
                            EffectKind::ProcessBoundary => EffectOutcome::Unit,
                            _ => {
                                let phase = Instant::now();
                                let op = rmp_serde::from_slice(&request.payload.0)?;
                                parent_decode = super::nanos(phase) as i64;
                                let phase = Instant::now();
                                let answer =
                                    host.perform(op).map_err(|e| anyhow::anyhow!("{e}"))?;
                                host_ns = super::nanos(phase) as i64;
                                let phase = Instant::now();
                                let bytes = rmp_serde::to_vec_named(&answer)?;
                                parent_encode = super::nanos(phase) as i64;
                                value_answer = Some(answer);
                                EffectOutcome::Value(EncodedPayload(bytes))
                            }
                        };
                        let next = worker.effect_result_measured(EffectResponse {
                            id: request.id,
                            outcome: result,
                        })?;
                        let elapsed = lash_vm_client::ipc::monotonic_nanos()?
                            .checked_sub(initial.response_started_ns)
                            .ok_or_else(|| anyhow::anyhow!("worker clock moved backwards"))?;
                        if timed {
                            let mut timing = worker.exchange_timing()?;
                            timing.worker_encode_ns += initial.worker_encode_ns;
                            anyhow::ensure!(
                                timing.worker_samples == 1,
                                "{} missing or duplicate worker timing",
                                case.name
                            );
                            observation.exchange_ns.push(elapsed);
                            observation
                                .exchange_phases
                                .push(super::metrics::Phases::measured(
                                    elapsed as i64,
                                    parent_decode + initial.response_decode_ns as i64,
                                    parent_encode,
                                    host_ns,
                                    timing,
                                ));
                            if case.name.starts_with("value-") {
                                observation.value_fixtures.push((
                                    request,
                                    value_answer.ok_or_else(|| {
                                        anyhow::anyhow!("value population has no answer")
                                    })?,
                                ));
                            }
                        }
                        next
                    }
                    WorkerMessage::Complete {
                        state: snapshot,
                        value,
                    } => {
                        outcome = Some(rmp_serde::from_slice(&value.0)?);
                        observation.state_bytes =
                            observation.state_bytes.max(snapshot.bytes().len() as u64);
                        state = StartState::Snapshot(snapshot);
                        break;
                    }
                    WorkerMessage::GuestError { .. } if case.error => break,
                    other => bail!("{}: unexpected worker message {other:?}", case.name),
                }
            }
            if sample_memory && !case.error {
                let memory = super::metrics::memory(observation.pid)?;
                observation.worker_rss_kib =
                    Some(observation.worker_rss_kib.unwrap_or(0).max(memory.0));
                observation.worker_peak_kib =
                    Some(observation.worker_peak_kib.unwrap_or(0).max(memory.1));
            }
            // GuestError has already been discarded and reaped by the pool.
            // Its post-work RSS is unavailable rather than recorded as zero.
            let reset = Instant::now();
            if case.error {
                drop(worker);
            } else {
                worker.release()?;
            }
            observation.resets_ns.push(super::nanos(reset));
            if !parked {
                break;
            }
        }
    }
    host.check(case, outcome.as_ref())?;
    Ok(observation)
}
