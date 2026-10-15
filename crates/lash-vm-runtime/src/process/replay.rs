//! A process's recorded run segments, replayed on demand (FIG-6370).
//!
//! A process observed at `counts` records, with each `kernel_run`'s end,
//! what its host reads answered and where it started: the state it
//! resumed and the admissions and outcomes it was handed, each by
//! reference to the retained record that holds it. A replay re-runs the
//! chosen segments on a worker from exactly that start, answers the reads
//! from the recorded tape, and records the statement path. Its end is
//! compared with the recorded end: a segment whose replay reaches it
//! byte for byte, with the same counts, is `Verified` with its path.
//! Anything else is `Diverged` or `Unreplayable`; a replay never answers a
//! path it did not verify.
//!
//! A replay is a read. It drives nothing and performs nothing: the run
//! binds every effect as recorded, and a park ends it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lash_core::{ProcessId, SettledOutput};
use lash_core_execution::runtime::process::steps::{RecordedProcessStep, process_step_call};
use lash_kernel_doc::{Datum, ErrorDatum, Handle, Integer, TaskIdentity, Timestamp};
use lash_kernel_vm::{Start, Step, Target};
use lash_vm_broker::ParentFault;
use lash_vm_broker::kernel::KernelFailure;
use lash_vm_client::wire::OutcomeWire;
use lash_vm_client::{FrameEpoch, InfrastructureOutcome, OwnerEpoch, RunHost, VmOwner};
use lash_vm_protocol::{GraphStateRefusal, KernelRestoreRefusal, RunRefusal};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::run::{Resumed, admission_of, drive, entry_args, outcome_of_end};
use super::state::{
    Admission, DeliverySource, HostReads, Issued, KERNEL_RUN_STEP, KernelProcessInput,
    KernelRunOutput, RecordedAdmission, RecordedDelivery, RecordedStart, SealedRun, StartedFrom,
    outcome_digest, refused,
};
use super::{KernelProcessEngine, KernelRecordedSettings};

/// Which of a process's run segments a replay re-runs, by their ordinal:
/// segment `n` is the process's `kernel_run.n`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaySegments {
    All,
    Only(std::collections::BTreeSet<u64>),
}

/// What a host asks a replay of: the process, its recorded settings and
/// current payload, the steps its run journal retains, and the segments.
#[derive(Clone, Debug)]
pub struct ProcessReplayRequest {
    pub process: ProcessId,
    pub settings: Option<lash_core::EngineSettings>,
    pub payload: serde_json::Value,
    pub steps: Vec<RecordedProcessStep>,
    pub segments: ReplaySegments,
}

/// One replayed segment's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentReplay {
    pub segment: u64,
    pub answer: SegmentAnswer,
}

/// Whether a segment's replay reached its recorded end.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum SegmentAnswer {
    /// The replay reached the recorded end and counts: this is the path
    /// the committed segment ran.
    Verified {
        path: ReplayPath,
    },
    /// The replay ran and did not reach the recorded end: a read its tape
    /// does not answer, or a recomputation that differs.
    Diverged,
    Unreplayable {
        reason: Unreplayable,
    },
}

/// Why a segment cannot be replayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unreplayable {
    /// The process, or a record its start refers to, is no longer retained.
    Pruned,
    /// The segment ran under a kernel version this build does not run.
    KernelVersion,
    /// The segment's run pins a helper release or function this build's
    /// workers do not hold.
    ReleaseMissing,
    /// The segment recorded no tape or start: it ran before they were
    /// recorded, or it reached no recorded end.
    NoTape,
    /// The process is observed `off`: its segments record nothing to
    /// replay.
    Unobserved,
}

/// A segment's statement path: each task's document statement starts and
/// empty-block entries, in order, as document site ordinals, tasks in the
/// order they first started one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayPath {
    pub tasks: Vec<TaskPath>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPath {
    pub task: TaskIdentity,
    pub starts: Vec<u32>,
}

impl ReplayPath {
    fn of(path: lash_vm_client::wire::OrdinalPath) -> Self {
        let mut tasks: Vec<TaskPath> = Vec::new();
        for (task, starts) in path {
            match tasks.iter_mut().find(|held| held.task == task) {
                Some(held) => held.starts.extend(starts),
                None => tasks.push(TaskPath { task, starts }),
            }
        }
        Self { tasks }
    }
}

/// Why a replay answered nothing: the worker or the store failed it, not
/// the segment.
#[derive(Debug, thiserror::Error)]
#[error("the replay failed: {0}")]
pub struct ReplayFault(String);

/// The host a replayed segment reads: its recorded tape. A read the tape
/// does not answer diverges the replay. A process binds no handle, so a
/// projection read is unreachable; meeting one diverges it too.
struct ReplayHost {
    now_ms: Option<i64>,
    random: std::sync::Mutex<std::collections::VecDeque<u64>>,
    clock_read: AtomicBool,
    diverged: AtomicBool,
}

impl ReplayHost {
    fn of(reads: &HostReads) -> Self {
        Self {
            now_ms: reads.now_ms,
            random: std::sync::Mutex::new(reads.random.iter().copied().collect()),
            clock_read: AtomicBool::new(false),
            diverged: AtomicBool::new(false),
        }
    }

    fn diverge(&self, why: &str) -> ParentFault {
        self.diverged.store(true, Ordering::Relaxed);
        ParentFault(format!("the replay diverged: {why}"))
    }

    /// Whether the run read exactly what its tape recorded.
    fn consumed(&self) -> bool {
        let draws = self
            .random
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        draws.is_empty()
            && self.clock_read.load(Ordering::Relaxed) == self.now_ms.is_some()
            && !self.diverged.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl RunHost for ReplayHost {
    fn observe(&self) -> lash_kernel_vm::Observe {
        lash_kernel_vm::Observe::Counts
    }

    fn traces_path(&self) -> bool {
        true
    }

    async fn clock(&self) -> Result<Timestamp, ParentFault> {
        let now_ms = self
            .now_ms
            .ok_or_else(|| self.diverge("the segment read a clock its tape lacks"))?;
        self.clock_read.store(true, Ordering::Relaxed);
        Ok(Timestamp {
            nanoseconds: Integer::from(now_ms.saturating_mul(1_000_000)),
        })
    }

    async fn random(&self) -> Result<u64, ParentFault> {
        self.random
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .ok_or_else(|| self.diverge("the segment drew more random bits than its tape holds"))
    }

    async fn read(
        &self,
        _handle: &Handle,
        _request: &Datum,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault> {
        Err(self.diverge("a process binds no handle, yet the segment read a projection"))
    }

    async fn perform(
        &self,
        effect: &lash_kernel_doc::EffectName,
        _args: &[Datum],
        _site: &lash_kernel_doc::Site,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault> {
        Err(self.diverge(&format!(
            "a kernel process binds no read, and the segment performed `{effect}` as one"
        )))
    }

    fn print(&self, _value: Datum) -> Result<(), ParentFault> {
        Ok(())
    }
}

/// The segment's start, resolved from the records it refers to.
struct Start_ {
    parked: Option<SealedRun>,
    admissions: Vec<Admission>,
    deliver: Vec<(u64, OutcomeWire)>,
}

/// What the journal retains of a process, as a replay reads it.
struct Journal<'a> {
    process: &'a ProcessId,
    steps: &'a [RecordedProcessStep],
    /// Each `kernel_run`'s recorded output, by its ordinal.
    outputs: Vec<Option<KernelRunOutput>>,
}

impl Journal<'_> {
    fn parked(&self, run: u64) -> Option<(&SealedRun, &[Issued])> {
        match self.outputs.get(usize::try_from(run).ok()?)?.as_ref()? {
            KernelRunOutput::Parked { state, issued, .. } => Some((state, issued)),
            KernelRunOutput::Ended { .. } => None,
        }
    }

    fn issued(&self, run: u64, wait: u64) -> Option<&Issued> {
        self.parked(run)?
            .1
            .iter()
            .find(|issued| issued.wait() == wait)
    }

    /// The outcome the engine handed the machine for `source`: read back
    /// from the record that holds it, and bound by its digest.
    fn delivered(
        &self,
        wait: u64,
        source: &DeliverySource,
        digest: &str,
    ) -> Result<OutcomeWire, Unreplayable> {
        let candidates = match source {
            DeliverySource::Elapsed => vec![OutcomeWire::Elapsed],
            DeliverySource::Refused { run } => match self.issued(*run, wait) {
                Some(Issued::Refused { kind, message, .. }) => vec![refused(kind, message.clone())],
                _ => return Err(Unreplayable::Pruned),
            },
            DeliverySource::Step { step } => {
                let Some(outcome) = self
                    .steps
                    .iter()
                    .find(|recorded| {
                        recorded.call == process_step_call(self.process, step, recorded.run)
                    })
                    .and_then(|recorded| recorded.outcome.as_ref())
                else {
                    return Err(Unreplayable::Pruned);
                };
                // A catalog tool step retains its tool record; the engine
                // was handed the record's output.
                let settled = lash_core_execution::runtime::process::steps::tool_step_settled(
                    self.process,
                    outcome.clone(),
                );
                [outcome.clone(), settled]
                    .iter()
                    .filter_map(super::advance::effect_outcome)
                    .map(OutcomeWire::from)
                    .collect()
            }
        };
        candidates
            .into_iter()
            .find(|outcome| outcome_digest(outcome) == digest)
            .ok_or(Unreplayable::NoTape)
    }

    fn start(&self, start: &RecordedStart) -> Result<Start_, Unreplayable> {
        let parked = match &start.from {
            StartedFrom::Entry => None,
            StartedFrom::Output { run, root } => match self.parked(*run) {
                Some((state, _)) if StartedFrom::root_of(state) == *root => Some(state.clone()),
                _ => return Err(Unreplayable::Pruned),
            },
            StartedFrom::Whole(state) => Some(state.clone()),
        };
        let admissions = start
            .admissions
            .iter()
            .map(|admission| match admission {
                RecordedAdmission::Issued { wait, run } => self
                    .issued(*run, *wait)
                    .map(|issued| issued.admission().clone())
                    .ok_or(Unreplayable::Pruned),
                RecordedAdmission::Whole(admission) => Ok((**admission).clone()),
            })
            .collect::<Result<_, _>>()?;
        let deliver = start
            .deliver
            .iter()
            .map(|delivery| match delivery {
                RecordedDelivery::Of {
                    wait,
                    source,
                    digest,
                } => self
                    .delivered(*wait, source, digest)
                    .map(|outcome| (*wait, outcome)),
                RecordedDelivery::Whole(delivery) => Ok((delivery.wait, delivery.outcome.clone())),
            })
            .collect::<Result<_, _>>()?;
        Ok(Start_ {
            parked,
            admissions,
            deliver,
        })
    }
}

/// The answer a run failure is to a replay, or the fault that leaves it
/// unanswered.
fn failed(host: &ReplayHost, failure: KernelFailure) -> Result<SegmentAnswer, ReplayFault> {
    if host.diverged.load(Ordering::Relaxed) {
        return Ok(SegmentAnswer::Diverged);
    }
    let unreplayable = |reason| Ok(SegmentAnswer::Unreplayable { reason });
    match &failure {
        KernelFailure::Worker {
            outcome: InfrastructureOutcome::RunRefused { refusal },
        } => match refusal {
            RunRefusal::Restore {
                refusal:
                    KernelRestoreRefusal::ReleaseMismatch { .. }
                    | KernelRestoreRefusal::MissingHelper { .. }
                    | KernelRestoreRefusal::MissingNative { .. }
                    | KernelRestoreRefusal::MissingFunction { .. },
            } => unreplayable(Unreplayable::ReleaseMissing),
            RunRefusal::Restore {
                refusal: KernelRestoreRefusal::KernelVersion { .. },
            }
            | RunRefusal::GraphState {
                refusal: GraphStateRefusal::KernelOutsideReadRange { .. },
            } => unreplayable(Unreplayable::KernelVersion),
            _ => Err(ReplayFault(failure.to_string())),
        },
        _ => Err(ReplayFault(failure.to_string())),
    }
}

impl KernelProcessEngine {
    /// Replays the chosen segments of a process from what its journal
    /// retains (FIG-6370). A segment past the last recorded one is not
    /// answered.
    ///
    /// # Errors
    ///
    /// [`ReplayFault`] when a worker or the document store fails a replay:
    /// nothing about the segment is known.
    pub async fn replay(
        &self,
        request: &ProcessReplayRequest,
    ) -> Result<Vec<SegmentReplay>, ReplayFault> {
        let kind = lash_core::EngineStepKind::new(KERNEL_RUN_STEP)
            .admitted_tool(crate::LASH_VM_ENGINE_KIND)
            .as_str()
            .to_owned();
        let runs: Vec<&RecordedProcessStep> = request
            .steps
            .iter()
            .filter(|step| step.tool == kind)
            .collect();
        let journal = Journal {
            process: &request.process,
            steps: &request.steps,
            outputs: runs
                .iter()
                .enumerate()
                .map(|(segment, step)| {
                    let name = lash_core::StepName(format!("{KERNEL_RUN_STEP}.{segment}"));
                    if step.call != process_step_call(&request.process, &name, step.run) {
                        return None;
                    }
                    match step.outcome.as_ref()? {
                        SettledOutput::Completed(material) => {
                            serde_json::from_str(material.payload()).ok()
                        }
                        _ => None,
                    }
                })
                .collect(),
        };
        let chosen: Vec<u64> = match &request.segments {
            ReplaySegments::All => (0..journal.outputs.len() as u64).collect(),
            ReplaySegments::Only(segments) => segments
                .iter()
                .copied()
                .filter(|segment| *segment < journal.outputs.len() as u64)
                .collect(),
        };
        let settings = request
            .settings
            .as_ref()
            .and_then(|settings| {
                serde_json::from_value::<KernelRecordedSettings>(settings.value().clone()).ok()
            })
            .filter(|settings| settings.observe == lash_kernel_vm::Observe::Counts);
        let mut answers = Vec::with_capacity(chosen.len());
        for segment in chosen {
            let answer = match &settings {
                None => SegmentAnswer::Unreplayable {
                    reason: Unreplayable::Unobserved,
                },
                Some(settings) => {
                    self.replay_segment(request, &journal, settings, segment)
                        .await?
                }
            };
            answers.push(SegmentReplay { segment, answer });
        }
        Ok(answers)
    }

    async fn replay_segment(
        &self,
        request: &ProcessReplayRequest,
        journal: &Journal<'_>,
        settings: &KernelRecordedSettings,
        segment: u64,
    ) -> Result<SegmentAnswer, ReplayFault> {
        let unreplayable = |reason| Ok(SegmentAnswer::Unreplayable { reason });
        let Some(recorded) = usize::try_from(segment)
            .ok()
            .and_then(|segment| journal.outputs.get(segment))
            .and_then(Option::as_ref)
        else {
            return unreplayable(Unreplayable::NoTape);
        };
        let observed = recorded.observed();
        let (Some(reads), Some(recorded_start)) = (observed.reads, observed.start) else {
            return unreplayable(Unreplayable::NoTape);
        };
        let start = match journal.start(recorded_start) {
            Ok(start) => start,
            Err(reason) => return unreplayable(reason),
        };
        let writes = self.writes.number();
        let recorded_state = match recorded {
            KernelRunOutput::Parked { state, .. } => Some(state),
            KernelRunOutput::Ended { .. } => None,
        };
        if start
            .parked
            .iter()
            .chain(recorded_state)
            .any(|state| state.0.kernel() != writes)
        {
            return unreplayable(Unreplayable::KernelVersion);
        }
        let document = match self.documents.get_shared(&recorded_start.document).await {
            Ok(Some(document)) => document,
            Ok(None) => return unreplayable(Unreplayable::Pruned),
            Err(error) => return Err(ReplayFault(error.to_string())),
        };
        if document.manifest.kernel != writes {
            return unreplayable(Unreplayable::KernelVersion);
        }
        let Ok(mut input) = KernelProcessInput::from_payload(&request.payload) else {
            return unreplayable(Unreplayable::NoTape);
        };
        input.document = recorded_start.document;
        let Ok(args) = entry_args(&document, &input) else {
            return unreplayable(Unreplayable::NoTape);
        };
        let host = Arc::new(ReplayHost::of(reads));
        let machines = lash_vm_client::RemoteMachines::new(
            self.workers.clone(),
            VmOwner::new(format!("process:{}", request.process)),
            OwnerEpoch(0),
            FrameEpoch(0),
            Arc::clone(&host) as Arc<dyn RunHost>,
            &document,
            &settings.dialect,
            Start {
                target: Target::Entry(input.entry.clone()),
                args,
                bindings: Default::default(),
            },
            lash_kernel_vm::EffectModes::recorded(),
            settings.bounds(),
        )
        .map_err(|error| ReplayFault(error.to_string()))?;
        let driven = drive(
            &machines,
            &request.process,
            &recorded_start.document.to_string(),
            Resumed {
                parked: start.parked,
                admissions: &start.admissions,
                deliver: start.deliver,
            },
            self.workers.config().tuning.slice,
            &CancellationToken::new(),
        )
        .await;
        let driven = match driven {
            Ok(driven) => driven,
            Err(failure) => return failed(&host, failure),
        };
        let reached = match (&driven.step, recorded) {
            (
                Step::Parked(park),
                KernelRunOutput::Parked {
                    state,
                    issued,
                    withdrawn,
                    ..
                },
            ) => {
                let replayed: Vec<u64> = park.withdrawn.iter().map(|wait| wait.0).collect();
                let asked: Vec<(u64, Admission)> = park
                    .requests
                    .iter()
                    .map(admission_of)
                    .filter(|(wait, _)| !replayed.contains(wait))
                    .collect();
                let recorded_asks: Vec<(u64, Admission)> = issued
                    .iter()
                    .map(|issued| (issued.wait(), issued.admission().clone()))
                    .collect();
                driven.state.as_ref() == Some(state)
                    && replayed == *withdrawn
                    && asked == recorded_asks
            }
            (Step::Ended(end), KernelRunOutput::Ended { outcome, .. }) => {
                let durable = |outcome: &lash_core::ProcessOutcome| {
                    serde_json::to_value(lash_sansio::Durable(outcome)).ok()
                };
                durable(&outcome_of_end(end.clone())) == durable(outcome)
            }
            _ => false,
        };
        Ok(
            if reached && host.consumed() && driven.ran == *observed.ran {
                SegmentAnswer::Verified {
                    path: ReplayPath::of(driven.path),
                }
            } else {
                SegmentAnswer::Diverged
            },
        )
    }
}
