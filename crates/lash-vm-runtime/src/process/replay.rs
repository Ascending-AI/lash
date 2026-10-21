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
//! binds every effect as recorded, and a park ends it. On the persistent
//! heap the state a segment resumed is the page the previous segment's
//! output published, which that output's record owns while the journal
//! retains it: the replay reads it under a reader's lease on that record,
//! and answers `Pruned` once the record is gone. What it seals at a park
//! is compared by its content-addressed root and never published.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use lash_core::ProcessId;
use lash_core_execution::runtime::process::steps::{
    RecordedProcessStep, StepFact, process_step_call,
};
use lash_core_store::tool_run::SettledOutput;
use lash_durable::domain::pages::PageStore;
use lash_durable::domain::{ExecKey, OwnerKey, RootOwner};
use lash_kernel_doc::{Datum, ErrorDatum, Handle, Integer, Timestamp};
use lash_kernel_vm::Target;
use lash_vm_broker::ParentFault;
use lash_vm_broker::kernel::{KernelFailure, MachineStep, Machines as _};
use lash_vm_client::wire::OutcomeWire;
use lash_vm_client::{FrameEpoch, InfrastructureOutcome, OwnerEpoch, RunHost, VmOwner};
use lash_vm_protocol::{CheckpointRefusal, KernelRestoreRefusal, RunRefusal};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::pages::{ParkedUnder, StepActivation};
use super::run::{admission_of, entry_args, outcome_of_end, run_machine};
use super::state::{
    Admission, DeliverySource, HostReads, Issued, KERNEL_RUN_STEP, KernelProcessInput,
    KernelRunOutput, ParkedPage, RecordedAdmission, RecordedDelivery, RecordedStart, StartedFrom,
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
/// current payload, the steps its run journal retains, the segments, and
/// the store whose pages those steps' records own.
#[derive(Clone)]
pub struct ProcessReplayRequest {
    pub pages: Arc<dyn PageStore>,
    pub process: ProcessId,
    pub settings: Option<lash_execution_contracts::process::EngineSettings>,
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
    /// The segment's run pins a function this build's workers do not
    /// hold, or whose native they lack.
    FunctionMissing,
    /// The segment recorded no tape or start: it ran before they were
    /// recorded, or it reached no recorded end.
    NoTape,
    /// The process is observed `off`: its segments record nothing to
    /// replay.
    Unobserved,
}

pub use lash_kernel_doc::{ReplayPath, TaskPath};

fn replay_path(path: lash_kernel_vm::ExecutionPath) -> ReplayPath {
    path.replay()
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
    effects: std::sync::Mutex<std::collections::VecDeque<super::state::EffectRead>>,
    clock_read: AtomicBool,
    diverged: AtomicBool,
}

impl ReplayHost {
    fn of(reads: &HostReads) -> Self {
        Self {
            now_ms: reads.now_ms,
            effects: std::sync::Mutex::new(reads.effects.iter().cloned().collect()),
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
        self.effects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
            && draws.is_empty()
            && self.clock_read.load(Ordering::Relaxed) == self.now_ms.is_some()
            && !self.diverged.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl RunHost for ReplayHost {
    fn observe(&self) -> lash_kernel_vm::ObservationMode {
        lash_kernel_vm::ObservationMode::Counts
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
        args: &[Datum],
        site: &lash_kernel_doc::PositionLabel,
        origin: &lash_kernel_doc::Site,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault> {
        let read = self
            .effects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .ok_or_else(|| self.diverge("the segment read beyond its operation tape"))?;
        if read.effect != *effect
            || read.args != args
            || read.site != *site
            || read.origin != *origin
        {
            return Err(self.diverge("the segment's operation read differs from its tape"));
        }
        Ok(read.answer)
    }

    fn print(&self, _value: Datum) -> Result<(), ParentFault> {
        Ok(())
    }
}

/// The segment's start, resolved from the records it refers to: the page
/// it resumed with the run fact whose record holds it.
struct Start_ {
    parked: Option<(ParkedPage, StepFact)>,
    admissions: Vec<Admission>,
    deliver: Vec<(lash_protocol_types::WaitId, OutcomeWire)>,
}

/// What the journal retains of a process, as a replay reads it.
struct Journal<'a> {
    process: &'a ProcessId,
    steps: &'a [RecordedProcessStep],
    /// Each `kernel_run`'s recorded output, by its ordinal, with the run
    /// fact that records it.
    outputs: Vec<Option<(KernelRunOutput, Option<StepFact>)>>,
}

impl Journal<'_> {
    fn output(&self, run: u64) -> Option<&(KernelRunOutput, Option<StepFact>)> {
        self.outputs.get(usize::try_from(run).ok()?)?.as_ref()
    }

    fn parked(&self, run: u64) -> Option<(&ParkedPage, &[Issued])> {
        match &self.output(run)?.0 {
            KernelRunOutput::Parked { state, issued, .. } => Some((state, issued)),
            KernelRunOutput::Ended { .. } => None,
        }
    }

    fn issued(&self, run: u64, wait: lash_protocol_types::WaitId) -> Option<&Issued> {
        self.parked(run)?
            .1
            .iter()
            .find(|issued| issued.wait() == wait)
    }

    /// The outcome the engine handed the machine for `source`: read back
    /// from the record that holds it, and bound by its digest.
    fn delivered(
        &self,
        wait: lash_protocol_types::WaitId,
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
            StartedFrom::Output { run, root } => {
                match (
                    self.parked(*run),
                    self.output(*run).and_then(|output| output.1),
                ) {
                    (Some((page, _)), Some(fact)) if page.root.to_hex() == *root => {
                        Some((*page, fact))
                    }
                    _ => return Err(Unreplayable::Pruned),
                }
            }
        };
        let admissions = start
            .admissions
            .iter()
            .map(|RecordedAdmission { wait, run }| {
                self.issued(*run, *wait)
                    .map(|issued| issued.admission().clone())
                    .ok_or(Unreplayable::Pruned)
            })
            .collect::<Result<_, _>>()?;
        let deliver = start
            .deliver
            .iter()
            .map(
                |RecordedDelivery {
                     wait,
                     source,
                     digest,
                 }| {
                    self.delivered(*wait, source, digest)
                        .map(|outcome| (*wait, outcome))
                },
            )
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
                    KernelRestoreRefusal::MissingNative { .. }
                    | KernelRestoreRefusal::DefinitionRefused { .. },
            } => unreplayable(Unreplayable::FunctionMissing),
            RunRefusal::Restore {
                refusal: KernelRestoreRefusal::KernelVersion { .. },
            }
            | RunRefusal::Checkpoint {
                refusal: CheckpointRefusal::KernelOutsideReadRange { .. },
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
        let kind = lash_execution_contracts::process::EngineStepKind::new(KERNEL_RUN_STEP)
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
                    let name = lash_execution_contracts::process::StepName(format!(
                        "{KERNEL_RUN_STEP}.{segment}"
                    ));
                    if step.call != process_step_call(&request.process, &name, step.run) {
                        return None;
                    }
                    match step.outcome.as_ref()? {
                        SettledOutput::Completed(material) => {
                            serde_json::from_str(material.payload())
                                .ok()
                                .map(|output| (output, step.outcome_fact))
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
            .filter(|settings| settings.observe == lash_kernel_vm::ObservationMode::Counts);
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
        let Some((recorded, _)) = journal.output(segment) else {
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
            .map(|(page, _)| page)
            .chain(recorded_state)
            .any(|page| page.kernel != writes)
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
        // A replay runs the code the run ran: the definitions its document
        // was written against, the store's where the node lacks them.
        let Ok(closure) = super::run::pinned_closure(self, &document)
            .await
            .map_err(|fault| ReplayFault(fault.0))?
        else {
            return unreplayable(Unreplayable::FunctionMissing);
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
            lash_vm_client::RunStart {
                target: Target::Entry(input.entry.clone()),
                args,
                session: None,
                modes: lash_kernel_vm::EffectModes::reading(reads.read_effects.iter().cloned()),
                host: Vec::new(),
                save: None,
            },
            settings.bounds(),
            closure,
        )
        .map_err(|error| ReplayFault(error.to_string()))?;
        let lease = self.policy.replay_lease;
        if lease.is_zero() || lease > super::KernelRunPolicy::MAX_REPLAY_LEASE {
            return Err(ReplayFault(format!(
                "the replay lease of {lease:?} is outside (0, {:?}]",
                super::KernelRunPolicy::MAX_REPLAY_LEASE
            )));
        }
        let binding = lash_vm_broker::kernel::graph::owner_binding_node(&ExecKey::Process(
            request.process.clone(),
        ))
        .map_err(|refusal| ReplayFault(refusal.to_string()))?;
        let config = self.workers.config();
        let spools = self
            .workers
            .spools()
            .map_err(|failure| ReplayFault(failure.to_string()))?;
        let closure = machines
            .closure()
            .map_err(|failure| ReplayFault(failure.to_string()))?;
        let parked = start.parked.map(|(page, fact)| {
            (
                page,
                ParkedUnder::Record {
                    owner: RootOwner::RunFact {
                        owner: OwnerKey::Process(request.process.clone()),
                        run: fact.run,
                        ordinal: fact.ordinal,
                    },
                    record: fact.record,
                    ttl: lease,
                },
            )
        });
        let opened = StepActivation::open(
            Arc::clone(&request.pages),
            &request.process,
            parked,
            start
                .admissions
                .iter()
                .map(|admission| admission.0.clone())
                .collect(),
            binding,
            closure,
            &spools,
            // A host-side replay runs outside the engine: it times its
            // reader's lease on the system clock.
            Arc::new(lash_core_execution::runtime::SystemClock),
            lash_vm_broker::kernel::pages::PageLimits::within(config.protocol.decode),
        )
        .await;
        let mut activation = match opened {
            Ok(activation) => activation,
            Err(KernelFailure::Lease(lash_durable::DurableError::Graph(
                lash_core_store::checkpoint_graph::GraphRefusal::RootRetired
                | lash_core_store::checkpoint_graph::GraphRefusal::RevisionConflict,
            ))) => return unreplayable(Unreplayable::Pruned),
            Err(failure) => return failed(&host, failure),
        };
        let driven = run_machine(
            &machines,
            &mut activation,
            binding,
            start.deliver,
            config.tuning.slice,
            &CancellationToken::new(),
        )
        .await;
        activation.end_reads().await;
        let driven = match driven {
            Ok(driven) => driven,
            Err(failure) => return failed(&host, failure),
        };
        let reached = match (&driven.step, recorded) {
            (
                MachineStep::Parked(park),
                KernelRunOutput::Parked {
                    state,
                    issued,
                    withdrawn,
                    ..
                },
            ) => {
                let replayed: Vec<lash_protocol_types::WaitId> = park.withdrawn.clone();
                let asked: Vec<(lash_protocol_types::WaitId, Admission)> = park
                    .requests
                    .iter()
                    .map(admission_of)
                    .filter(|(wait, _)| !replayed.contains(wait))
                    .collect();
                let recorded_asks: Vec<(lash_protocol_types::WaitId, Admission)> = issued
                    .iter()
                    .map(|issued| (issued.wait(), issued.admission().clone()))
                    .collect();
                driven.parked.as_ref().map(|(page, _)| page) == Some(state)
                    && replayed == *withdrawn
                    && asked == recorded_asks
            }
            (MachineStep::Ended(end), KernelRunOutput::Ended { outcome, .. }) => {
                let durable = |outcome: &lash_execution_contracts::process::ProcessOutcome| {
                    serde_json::to_value(lash_protocol_types::Durable(outcome)).ok()
                };
                durable(&outcome_of_end(end.clone())) == durable(outcome)
            }
            _ => false,
        };
        Ok(
            if reached && host.consumed() && driven.ran == *observed.ran {
                SegmentAnswer::Verified {
                    path: replay_path(driven.path),
                }
            } else {
                SegmentAnswer::Diverged
            },
        )
    }
}
