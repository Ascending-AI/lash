//! One workflow projection for hosts: committed process facts and the live
//! observations on that process's feed share task/site/occurrence identities.
//! Session cells still use the trace overlay: they have no process event log.
use std::collections::{BTreeMap, BTreeSet};

use lash_trace::{TraceLanguageExecutionPayload, TraceNodeFact, TraceRuntimeSubject};
use lash_vocabulary::names::LinkedStart;
use lash_vocabulary::sites::EffectId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::process_replay::ProcessObservationEventPayload;
use lash_execution_contracts::process::ObservedProcessEvent;
use lash_execution_contracts::process::ProcessEffectOutcomeClass;
use lash_execution_contracts::process::ProcessLifecycleFact;
use lash_execution_contracts::process::WaitKind;
use lash_vocabulary::identity::ProcessId;

/// What was observed at a site's latest occurrence. An unobserved terminal
/// does not claim the action succeeded: the process ended without that fact.
///
/// `Recovered` is a failed occurrence whose error a `catch` took, so the
/// run went on (FIG-6817): the committed `process.ran` of the run segment
/// that caught it names the occurrence, and its `process.effect_outcome`
/// stays a failure. `Failed` is a failure no `catch` recovered from: it
/// ended its task, or the catch that took it raised again before it
/// waited. Only a run observed at `counts` records its catches; at `off`
/// a caught failure stays `Failed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessWorkflowSiteState {
    Running,
    /// The occurrence is parked on `kind`. A sleep also carries
    /// `resume_at_ms`: the store-clock millisecond its park resumes at, the
    /// `until_ms` its `process.waiting` recorded (the live feed reports the
    /// same instant). It is the park's own instant, so a computed duration
    /// has one too, and replay and a reload read the same value. Every
    /// other wait leaves it `None`.
    Waiting {
        kind: ProcessWorkflowWaitKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_at_ms: Option<i64>,
    },
    Completed,
    Failed,
    Recovered,
    Cancelled,
    TerminalUnobserved,
}

/// The kind of blocker at a workflow site. No bearer key is exposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessWorkflowWaitKind {
    Sleep,
    Key,
    Process,
    Effect,
    Task,
}

impl From<&WaitKind> for ProcessWorkflowWaitKind {
    fn from(kind: &WaitKind) -> Self {
        match kind {
            WaitKind::Sleep { .. } => Self::Sleep,
            WaitKind::Key { .. } => Self::Key,
            WaitKind::Process { .. } => Self::Process,
            WaitKind::Call { .. } => Self::Effect,
        }
    }
}

/// One task's latest visit to an authored origin site. The VM's zero-based occurrence
/// index counts entries, including sleeps and calls, independently of effects.
///
/// Lash stores no wall time on an occurrence. One effect attempt's start,
/// end and result join, for every host, on its [`EffectId`] (the task, site,
/// occurrence and loops of `EffectIdentity`) across the process events the
/// host stores, each timed by its own `occurred_at_ms`:
/// - its result and end are the committed `process.effect_outcome` whose
///   `at` is that id: outcome class, failure code and, for a tool, the
///   `call_id` that names the call's record and answer;
/// - its start is the committed `process.waiting` whose `wait.site` is that
///   id, when the call was still in flight as the process released, and its
///   `process.resumed` ends that wait. A call that settles inside the run
///   segment that issued it commits no start: the host's tool
///   implementation sees its `call_id` when it begins.
///
/// A sleep is entered by its `process.waiting` and settled by its
/// `process.resumed`. A wait the process's cancel cut short ends with
/// `process.wait_cancelled` instead, which settles its occurrence as
/// [`ProcessWorkflowSiteState::Cancelled`]: a cancelled sleep never reads
/// completed. An occurrence past the evidence cap has no outcome of
/// its own; `process.effect_omissions` counts it. `first_entered` and
/// `last_settled` are the sequences of the first and the latest of these
/// events across the row's occurrences, so they bound the site's history,
/// not one attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessWorkflowSite {
    /// The authored origin used to group this row. `at` keeps the performing
    /// helper occurrence, independently of its placement in authored code.
    pub origin: lash_vocabulary::sites::Site,
    pub at: EffectId,
    /// A display label supplied by the engine, independent of the opaque token.
    pub label: String,
    pub occurrences: u64,
    pub state: ProcessWorkflowSiteState,
    /// Sequence of the first committed event naming an entry at this site,
    /// across occurrences. Live-only evidence has no sequence and leaves this
    /// `None`. Lash stores no wall time here: hosts map the sequence to the
    /// `occurred_at_ms` of the event they already hold.
    pub first_entered: Option<u64>,
    /// Sequence of the latest committed sleep resumption, cancelled wait or
    /// effect outcome settling an occurrence at this site. Retained through later entries;
    /// live-only completion and `TerminalUnobserved` supply no settlement.
    /// Always `None` until `first_entered` is present. Lash stores no wall time
    /// here: hosts map the sequence to their event's `occurred_at_ms`.
    pub last_settled: Option<u64>,
}

/// Which provisional evidence this fold holds. RetainedWindow is bounded;
/// it never claims to be the execution's entire history.
///
/// Only live observations from the process feed move a fold off
/// `NotObserved`: a fold of committed events alone
/// ([`ProcessWorkflowOverlayAccumulator::committed`]) answers `NotObserved`
/// however far the run got. A host that shows live coverage reads the
/// overlay of `core.processes().observe(id).snapshot()`, or folds the feed's
/// items with [`ProcessWorkflowOverlayAccumulator::observe`]; with more than
/// one replica it also installs a shared process replay store (on
/// PostgreSQL, `PostgresHost::process_replay()`), or a replica reading a run
/// another replica executes finds no frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessWorkflowLiveCoverage {
    /// The fold took no live observation.
    NotObserved,
    /// The fold holds the frames the replay window retained.
    RetainedWindow,
    /// The feed lost frames the fold would have taken.
    Gap {
        reason: super::process_replay::ProcessReplayGapReason,
    },
}

pub use lash_vocabulary::observation::RunObservation as ProcessWorkflowObservation;

/// How many times a document statement started, an empty block was entered,
/// an action issued or a while condition was tested, by its document count ordinal. `committed` sums the process's
/// `process.ran` events; `provisional` is what live frames reported since
/// the latest of them, which the next commit replaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessWorkflowRan {
    pub ordinal: u32,
    pub committed: u64,
    pub provisional: u64,
}

/// The workflow part of a process observation. Durable coverage is the
/// enclosing snapshot's `effects.coverage`; live entries are provisional and
/// can be lost at a replay gap. `current_sites` retains the last entered site
/// of each task through pure computation and is empty once the process ends.
///
/// At `observation: counts`, `ran` holds each ordinal that ran, and an
/// ordinal absent from it ran zero times. At `off` there is no `ran`: the
/// process is unobserved. Ordinals refer to the document the process runs
/// now: a successor carry restarts them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessWorkflowOverlay {
    pub process_id: ProcessId,
    /// Supplied by an accepted live observation, absent in a committed-only fold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<LinkedStart>,
    pub current_sites: Vec<EffectId>,
    pub live_coverage: ProcessWorkflowLiveCoverage,
    pub sites: Vec<ProcessWorkflowSite>,
    pub observation: ProcessWorkflowObservation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ran: Vec<ProcessWorkflowRan>,
}

impl ProcessWorkflowOverlay {
    /// The counts a host passes to `workflow::view::coverage`, including live
    /// provisional starts. Off remains unobserved even when the vector is empty.
    pub fn ran_counts(&self) -> lash_vocabulary::observation::RanCounts {
        lash_vocabulary::observation::RanCounts {
            observation: Some(self.observation),
            ran: self
                .ran
                .iter()
                .map(|row| (row.ordinal, row.committed.saturating_add(row.provisional)))
                .collect(),
        }
    }
}

/// An observation from another execution cannot enter a process projection.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProcessWorkflowFoldError {
    #[error("the observation names another process or workflow document")]
    DifferentExecution,
}

struct Site {
    value: ProcessWorkflowSite,
    /// A committed park or outcome wins over late provisional facts of that occurrence.
    durable_state: bool,
    /// Origin metadata does not establish transition ordering.
    timestamp: Option<u64>,
}

/// The single process workflow reducer. Feed stored events to `committed`,
/// and process-feed payloads to `observe`; neither requires diagnostic traces.
/// Replay duplicates and effect retries never increment the entry count.
pub struct ProcessWorkflowOverlayAccumulator {
    process_id: ProcessId,
    document: Option<LinkedStart>,
    sites: BTreeMap<lash_trace::WorkflowTaskSite, Site>,
    // At most one pending/latest origin per actual task/site. Live starts can
    // precede their origin metadata; no row is placed until the origin arrives.
    origins: BTreeMap<lash_trace::WorkflowTaskSite, (EffectId, lash_vocabulary::sites::Site)>,
    pending: BTreeMap<lash_trace::WorkflowTaskSite, (EffectId, u64, ProcessWorkflowSiteState)>,
    current: BTreeMap<lash_vocabulary::sites::TaskLabel, EffectId>,
    live_tasks: BTreeSet<lash_vocabulary::sites::TaskLabel>,
    live_entries: lash_vocabulary::sites::ObservationReader,
    through: u64,
    ended: bool,
    live_coverage: ProcessWorkflowLiveCoverage,
    observation: ProcessWorkflowObservation,
    /// Committed and provisional counts by ordinal.
    ran: BTreeMap<u32, (u64, u64)>,
}

impl ProcessWorkflowOverlayAccumulator {
    /// Start a fold of stored process events without loading a workflow document.
    /// Live observations supply their document reference to [`Self::observe`].
    pub fn new(process_id: ProcessId) -> Self {
        Self {
            process_id,
            document: None,
            sites: BTreeMap::new(),
            origins: BTreeMap::new(),
            pending: BTreeMap::new(),
            current: BTreeMap::new(),
            live_tasks: BTreeSet::new(),
            live_entries: lash_vocabulary::sites::ObservationReader::default(),
            through: 0,
            ended: false,
            live_coverage: ProcessWorkflowLiveCoverage::NotObserved,
            observation: ProcessWorkflowObservation::Off,
            ran: BTreeMap::new(),
        }
    }

    /// This fold of a process whose start recorded `observation`. A
    /// committed `process.ran` or a live count also says the process
    /// counts: only a counted run writes one.
    #[must_use]
    pub fn observing(mut self, observation: ProcessWorkflowObservation) -> Self {
        self.observation = observation;
        self
    }

    fn counted(&mut self, delta: &[(u32, u64)], committed: bool) {
        self.observation = ProcessWorkflowObservation::Counts;
        if committed {
            // A commit makes the run's counts exact: what live frames
            // reported before it is in it.
            for (_, provisional) in self.ran.values_mut() {
                *provisional = 0;
            }
        }
        for &(ordinal, count) in delta {
            let held = self.ran.entry(ordinal).or_default();
            if committed {
                held.0 = held.0.saturating_add(count);
            } else {
                held.1 = held.1.saturating_add(count);
            }
        }
    }

    /// Report a lost provisional window without discarding committed site evidence.
    pub fn live_gap(&mut self, reason: super::process_replay::ProcessReplayGapReason) {
        self.current.clear();
        self.pending.clear();
        self.origins.clear();
        self.live_tasks.clear();
        self.live_entries.clear();
        self.live_coverage = ProcessWorkflowLiveCoverage::Gap { reason };
    }

    fn enter_committed(&mut self, at: &EffectId) {
        // Live entries are ordered by the process feed, not by wall clocks
        // on successive owners. Durable facts supply the fallback when no
        // live entry for this task survived retention.
        if !self.ended && !self.live_tasks.contains(&at.task()) {
            self.current.insert(at.task(), at.clone());
        }
    }

    fn enter_live(&mut self, at: &EffectId) {
        if self.ended {
            return;
        }
        for at in self.live_entries.fold([at.clone()]) {
            self.live_tasks.insert(at.task());
            self.current.insert(at.task(), at);
        }
    }

    fn site(
        &mut self,
        at: &EffectId,
        origin: &lash_vocabulary::sites::Site,
        timestamp: u64,
        state: ProcessWorkflowSiteState,
        durable_terminal: bool,
        sequence: Option<u64>,
    ) {
        let actual = lash_trace::WorkflowTaskSite::of(at);
        if self
            .origins
            .get(&actual)
            .is_none_or(|(known, _)| known.visit() <= at.visit())
        {
            self.origins.insert(actual, (at.clone(), origin.clone()));
        }
        let key = lash_trace::WorkflowTaskSite {
            task: at.task(),
            site: origin.position(),
        };
        let durable_state = durable_terminal
            || (sequence.is_some() && matches!(state, ProcessWorkflowSiteState::Waiting { .. }));
        let held = self.sites.entry(key).or_insert_with(|| Site {
            value: ProcessWorkflowSite {
                origin: origin.clone(),
                at: at.clone(),
                label: at.to_string(),
                occurrences: lash_vocabulary::sites::ObservationReader::counts(at).starts,
                state: state.clone(),
                first_entered: None,
                last_settled: None,
            },
            durable_state,
            timestamp: Some(timestamp),
        });
        if (at.position() == held.value.at.position() && at.visit() > held.value.at.visit())
            || ((at.position() != held.value.at.position() || at.visit() == held.value.at.visit())
                && (sequence.is_some()
                    || (!held.durable_state
                        && held.timestamp.is_none_or(|held| timestamp >= held))))
        {
            held.value.origin = origin.clone();
            held.value.at = at.clone();
            held.value.label = at.to_string();
            held.value.occurrences = lash_vocabulary::sites::ObservationReader::counts(at).starts;
            held.value.state = state;
            held.durable_state = durable_state;
            held.timestamp = Some(timestamp);
        }
        // Committed history belongs to the site across all its occurrences,
        // even when a newer live occurrence already owns the displayed state.
        if let Some(sequence) = sequence {
            held.value.first_entered.get_or_insert(sequence);
            if durable_terminal {
                held.value.last_settled = Some(sequence);
            }
        }
    }

    /// A `catch` recovered from the failure of occurrence `at`: its row,
    /// while it shows that failed occurrence, shows it recovered. An
    /// occurrence past the evidence cap has no row of its own.
    fn recovered(&mut self, at: &EffectId) {
        for held in self.sites.values_mut() {
            if held.value.at == *at && held.value.state == ProcessWorkflowSiteState::Failed {
                held.value.state = ProcessWorkflowSiteState::Recovered;
            }
        }
    }

    fn live_site(&mut self, at: &EffectId, timestamp: u64, state: ProcessWorkflowSiteState) {
        let actual = lash_trace::WorkflowTaskSite::of(at);
        if let Some((known, origin)) = self.origins.get(&actual).cloned() {
            if known == *at {
                self.site(at, &origin, timestamp, state, false, None);
                return;
            }
            if known.visit() > at.visit() {
                return;
            }
        }
        if self
            .pending
            .get(&actual)
            .is_none_or(|(known, _, _)| known.visit() <= at.visit())
        {
            self.pending.insert(actual, (at.clone(), timestamp, state));
        }
    }

    fn origin_site(&mut self, at: &EffectId, origin: &lash_vocabulary::sites::Site) {
        let actual = lash_trace::WorkflowTaskSite::of(at);
        if self
            .origins
            .get(&actual)
            .is_some_and(|(known, _)| known.visit() > at.visit())
        {
            return;
        }
        self.origins
            .insert(actual.clone(), (at.clone(), origin.clone()));
        if self
            .pending
            .get(&actual)
            .is_some_and(|(known, _, _)| known == at)
            && let Some((_, timestamp, state)) = self.pending.remove(&actual)
        {
            self.site(at, origin, timestamp, state, false, None);
        }
    }

    /// A host's stored, typed lifecycle event, in sequence order. An already
    /// folded sequence is ignored, including commits replayed after a snapshot.
    ///
    /// This accepts the durable [`ObservedProcessEvent`], whose decoder expects
    /// internal terminal records. A host storing projected terminal payloads
    /// (from `ProcessTerminal::projection` or `ObservedProcessEvent::projection`)
    /// calls [`Self::finish`] when the stored event's kind is terminal, rather
    /// than decoding that payload as an `ObservedProcessEvent`.
    pub fn committed(&mut self, event: &ObservedProcessEvent) {
        if event.sequence <= self.through {
            return;
        }
        self.through = event.sequence;
        let now = event.occurred_at_ms;
        match &event.fact {
            ProcessLifecycleFact::Waiting { wait }
            | ProcessLifecycleFact::Resumed { wait }
            | ProcessLifecycleFact::WaitCancelled { wait } => {
                let Some(location) = &wait.site else {
                    return;
                };
                let at = &location.at;
                self.enter_committed(at);
                let sleep = matches!(wait.kind, WaitKind::Sleep { .. });
                // A sleep's end settles its occurrence, and so does a cancel
                // that cut any wait short; a resumed call runs on.
                let (state, settled) = match &event.fact {
                    ProcessLifecycleFact::Waiting { .. } => (
                        ProcessWorkflowSiteState::Waiting {
                            kind: (&wait.kind).into(),
                            resume_at_ms: match wait.kind {
                                WaitKind::Sleep { until_ms } => Some(until_ms),
                                WaitKind::Call { resume_at_ms, .. } => resume_at_ms,
                                _ => None,
                            },
                        },
                        false,
                    ),
                    ProcessLifecycleFact::WaitCancelled { .. } => {
                        (ProcessWorkflowSiteState::Cancelled, true)
                    }
                    _ if sleep => (ProcessWorkflowSiteState::Completed, true),
                    _ => (ProcessWorkflowSiteState::Running, false),
                };
                self.site(
                    at,
                    &location.origin,
                    now,
                    state,
                    settled,
                    Some(event.sequence),
                );
            }
            ProcessLifecycleFact::EffectOutcome(outcome) => {
                self.enter_committed(&outcome.at);
                let state = match outcome.outcome_class {
                    ProcessEffectOutcomeClass::Success => ProcessWorkflowSiteState::Completed,
                    ProcessEffectOutcomeClass::Failure => ProcessWorkflowSiteState::Failed,
                    ProcessEffectOutcomeClass::Cancelled => ProcessWorkflowSiteState::Cancelled,
                };
                self.site(
                    &outcome.at,
                    &outcome.origin,
                    now,
                    state,
                    true,
                    Some(event.sequence),
                );
            }
            ProcessLifecycleFact::Ran(ran) => {
                self.counted(&ran.ran, true);
                for at in &ran.caught {
                    self.recovered(at);
                }
            }
            // Ordinals from here on refer to the successor's document.
            ProcessLifecycleFact::Succeeded(_) => self.ran.clear(),
            ProcessLifecycleFact::Terminal { .. } => self.finish(),
            _ => {}
        }
    }

    /// Fold a known terminal when the host holds only its projection. After
    /// folding preceding committed events in sequence order, call this for any
    /// terminal kind (`event_type.terminal_status().is_some()` on a projected
    /// event); no terminal payload or workflow document is needed.
    ///
    /// Settled site outcomes remain authoritative. Running and waiting sites
    /// become [`ProcessWorkflowSiteState::TerminalUnobserved`]: the process's
    /// terminal does not prove that each unfinished action succeeded. Delayed
    /// live observations cannot leave a terminal process's sites running.
    pub fn finish(&mut self) {
        self.ended = true;
        self.current.clear();
        // Every run committed before the terminal: nothing is provisional.
        for (_, provisional) in self.ran.values_mut() {
            *provisional = 0;
        }
        self.ran.retain(|_, (committed, _)| *committed > 0);
        for site in self.sites.values_mut() {
            if matches!(
                site.value.state,
                ProcessWorkflowSiteState::Running | ProcessWorkflowSiteState::Waiting { .. }
            ) {
                site.value.state = ProcessWorkflowSiteState::TerminalUnobserved;
            }
        }
    }

    fn provisional_counts(&mut self, counts: &lash_trace::TraceProcessCounts) {
        if !self.ended {
            self.counted(&counts.delta, false);
        }
    }

    fn live_document(&mut self, document: &LinkedStart) -> Result<(), ProcessWorkflowFoldError> {
        if self.document.as_ref().is_some_and(|held| held != document) {
            return Err(ProcessWorkflowFoldError::DifferentExecution);
        }
        self.document = Some(document.clone());
        Ok(())
    }

    /// Fold exactly what a process feed emits. `document` is the expected
    /// reference for live payloads; accepted live evidence binds the fold to it.
    /// Committed payloads do not use or retain it. Hosts folding only stored
    /// events use [`Self::committed`] and need no reference.
    ///
    /// The process's committed terminal stays authoritative when delayed live
    /// observations arrive. Nested effect executions leave this fold unchanged.
    pub fn observe(
        &mut self,
        document: &LinkedStart,
        event: &ProcessObservationEventPayload,
    ) -> Result<(), ProcessWorkflowFoldError> {
        match event {
            ProcessObservationEventPayload::Committed { event } => self.committed(event),
            ProcessObservationEventPayload::StepBodyStarted(observation) => {
                if observation.step.process_id != self.process_id {
                    return Err(ProcessWorkflowFoldError::DifferentExecution);
                }
                self.live_document(document)?;
                self.live_coverage = ProcessWorkflowLiveCoverage::RetainedWindow;
                self.enter_live(&observation.step.at);
                self.site(
                    &observation.step.at,
                    &observation.step.origin,
                    observation.observed_at_ms,
                    ProcessWorkflowSiteState::Running,
                    false,
                    None,
                );
            }
            ProcessObservationEventPayload::LanguageExecution(observation) => {
                let execution = &observation.execution;
                // A process feed also carries its effects' nested language
                // executions. Their sites belong to their own documents;
                // the parent is still at the step that admitted the effect.
                if let TraceRuntimeSubject::Effect { address, .. } = &execution.subject()
                    && matches!(&address.execution_scope,
                        lash_protocol_types::ExecutionScope::Process { process_id } if process_id == self.process_id)
                {
                    return Ok(());
                }
                if !matches!(&execution.identity.document, lash_trace::TraceExecutionCode::Start(start) if start == document)
                    || !matches!(&execution.subject(),
                    TraceRuntimeSubject::Process { process_id } if process_id == self.process_id)
                {
                    return Err(ProcessWorkflowFoldError::DifferentExecution);
                }
                self.live_document(document)?;
                self.live_coverage = ProcessWorkflowLiveCoverage::RetainedWindow;
                if let TraceLanguageExecutionPayload::Counts { counts } = &execution.payload {
                    match counts {
                        lash_trace::TraceCountObservation::Process(counts) => {
                            self.provisional_counts(counts)
                        }
                        lash_trace::TraceCountObservation::Cell(_) => {}
                    }
                }
                if let TraceLanguageExecutionPayload::Node { at, fact, .. } = &execution.payload {
                    if let TraceNodeFact::Origin { origin } = fact {
                        self.origin_site(at, origin);
                        if self.ended {
                            self.finish();
                        }
                        return Ok(());
                    }
                    let state = match fact {
                        TraceNodeFact::Started { .. } => {
                            self.enter_live(at);
                            ProcessWorkflowSiteState::Running
                        }
                        TraceNodeFact::Completed { .. } => ProcessWorkflowSiteState::Completed,
                        TraceNodeFact::Failed { .. } => ProcessWorkflowSiteState::Failed,
                        TraceNodeFact::Cancelled => ProcessWorkflowSiteState::Cancelled,
                        TraceNodeFact::Waiting { awaited } => ProcessWorkflowSiteState::Waiting {
                            resume_at_ms: match awaited {
                                lash_trace::TraceNodeAwaited::Sleep { deadline_ms } => {
                                    deadline_ms.and_then(|at| i64::try_from(at).ok())
                                }
                                _ => None,
                            },
                            kind: match awaited {
                                lash_trace::TraceNodeAwaited::Sleep { .. } => {
                                    ProcessWorkflowWaitKind::Sleep
                                }
                                lash_trace::TraceNodeAwaited::Signal { .. } => {
                                    ProcessWorkflowWaitKind::Key
                                }
                                lash_trace::TraceNodeAwaited::ChildProcesses { .. } => {
                                    ProcessWorkflowWaitKind::Process
                                }
                                lash_trace::TraceNodeAwaited::ToolBatch { .. } => {
                                    ProcessWorkflowWaitKind::Effect
                                }
                                lash_trace::TraceNodeAwaited::RunAggregate { .. } => {
                                    ProcessWorkflowWaitKind::Task
                                }
                            },
                        },
                        TraceNodeFact::Resumed { .. } => ProcessWorkflowSiteState::Running,
                        _ => return Ok(()),
                    };
                    self.live_site(at, observation.observed_at_ms, state);
                }
            }
        }
        // A terminal may have preceded these provisional facts.
        if self.ended {
            self.finish();
        }
        Ok(())
    }

    pub fn snapshot(&self) -> ProcessWorkflowOverlay {
        ProcessWorkflowOverlay {
            process_id: self.process_id.clone(),
            document: self.document.clone(),
            live_coverage: self.live_coverage,
            current_sites: self.current.values().cloned().collect(),
            sites: self.sites.values().map(|site| site.value.clone()).collect(),
            observation: self.observation,
            ran: match self.observation {
                ProcessWorkflowObservation::Off => Vec::new(),
                ProcessWorkflowObservation::Counts => self
                    .ran
                    .iter()
                    .map(|(&ordinal, &(committed, provisional))| ProcessWorkflowRan {
                        ordinal,
                        committed,
                        provisional,
                    })
                    .collect(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_trace::{
        LanguageExecutionObservation, TraceLanguageExecution, TraceLanguageExecutionIdentity,
        TraceRuntimeScope,
    };

    /// FIG-6358: committed entry and settlement sequences survive re-entry,
    /// duplicate replay and reversed wall clocks.
    #[test]
    fn committed_site_sequences_retain_first_entry_and_latest_settlement() {
        let mut overlay =
            ProcessWorkflowOverlayAccumulator::new(ProcessId::fixture("site-sequences"));
        let mut at = lash_protocol_types::effect_identity_fixture("sleep", 0);
        for (sequence, occurrence, waiting, first_entered, last_settled) in [
            (7, 0, true, 7, None),
            (11, 0, false, 7, Some(11)),
            (15, 1, true, 7, Some(11)),
            (19, 1, false, 7, Some(19)),
            (19, 1, false, 7, Some(19)),
        ] {
            at = lash_vocabulary::sites::testkit::with_visit(&at, occurrence);
            let wait = lash_execution_contracts::process::WaitState {
                kind: WaitKind::Sleep { until_ms: 100 },
                since_ms: 1,
                site: Some(lash_vocabulary::sites::EffectLocation::authored(at.clone())),
            };
            overlay.committed(&ObservedProcessEvent {
                sequence,
                occurred_at_ms: 100 - sequence,
                fact: if waiting {
                    ProcessLifecycleFact::Waiting { wait }
                } else {
                    ProcessLifecycleFact::Resumed { wait }
                },
            });
            let snapshot = serde_json::to_value(overlay.snapshot()).expect("overlay wire shape");
            let site = &snapshot["sites"][0];
            assert_eq!(site["first_entered"], serde_json::json!(first_entered));
            assert_eq!(site["last_settled"], serde_json::json!(last_settled));
        }
    }

    /// FIG-6358: provisional evidence has no committed sequence, including
    /// a live completion; a later commit supplies both sequence coordinates.
    fn live_only_site_fixture() -> serde_json::Value {
        let process = ProcessId::fixture("live-site-sequences");
        let document = LinkedStart {
            link: lash_vocabulary::content_ids::LinkId::from_bytes([7; 32]),
            selector: lash_vocabulary::names::StartSelector::Main,
        };
        let mut overlay = ProcessWorkflowOverlayAccumulator::new(process.clone());
        let at = lash_protocol_types::effect_identity_fixture("effect", 0);
        let mut observation = LanguageExecutionObservation {
            language: None,
            observed_at_ms: 100,
            execution: TraceLanguageExecution {
                event_key: "live-site".into(),
                identity: TraceLanguageExecutionIdentity {
                    scope: TraceRuntimeScope::none(),
                    document: lash_trace::TraceExecutionCode::Start(document.clone()),
                    entry_name: "main".into(),
                    engine_execution_id: None,
                    generation: None,
                },
                payload: TraceLanguageExecutionPayload::Node {
                    subject: TraceRuntimeSubject::Process {
                        process_id: process,
                    },
                    at: at.clone(),
                    fact: TraceNodeFact::Started { call_id: None },
                },
            },
        };
        for fact in [
            TraceNodeFact::Origin {
                origin: at.position().site().clone(),
            },
            TraceNodeFact::Started { call_id: None },
            TraceNodeFact::Cancelled,
        ] {
            observation.execution.payload = TraceLanguageExecutionPayload::Node {
                subject: observation.execution.subject(),
                at: at.clone(),
                fact,
            };
            overlay
                .observe(
                    &document,
                    &ProcessObservationEventPayload::LanguageExecution(observation.clone()),
                )
                .expect("live evidence");
            if matches!(
                observation.execution.payload,
                TraceLanguageExecutionPayload::Node {
                    fact: TraceNodeFact::Origin { .. },
                    ..
                }
            ) {
                continue;
            }
            let snapshot = serde_json::to_value(overlay.snapshot()).expect("overlay wire shape");
            let site = snapshot["sites"][0].as_object().expect("site");
            for field in ["first_entered", "last_settled"] {
                assert_eq!(site.get(field), Some(&serde_json::Value::Null));
            }
        }
        overlay.committed(&ObservedProcessEvent {
            sequence: 7,
            occurred_at_ms: 1,
            fact: ProcessLifecycleFact::EffectOutcome(
                lash_execution_contracts::process::ProcessEffectOccurrence {
                    vocabulary_version:
                        lash_execution_contracts::process::PROCESS_EVENT_VOCABULARY_VERSION,
                    origin: at.position().site().clone(),
                    at: at.clone(),
                    operation: "fixture".into(),
                    outcome_class: ProcessEffectOutcomeClass::Failure,
                    code: None,
                    call_id: None,
                    replay_key: "fixture".into(),
                },
            ),
        });
        let snapshot = overlay.snapshot();
        assert_eq!(snapshot.sites[0].first_entered, Some(7));
        assert_eq!(snapshot.sites[0].last_settled, Some(7));

        // A later live occurrence owns the displayed state before the prior
        // occurrence's next committed settlement reaches this consumer.
        let mut next = at.clone();
        next = lash_vocabulary::sites::testkit::with_visit(&next, 1);
        observation.execution.payload = TraceLanguageExecutionPayload::Node {
            subject: observation.execution.subject(),
            at: next.clone(),
            fact: TraceNodeFact::Origin {
                origin: next.position().site().clone(),
            },
        };
        overlay
            .observe(
                &document,
                &ProcessObservationEventPayload::LanguageExecution(observation.clone()),
            )
            .expect("the new entry carries its authored origin");
        observation.execution.payload = TraceLanguageExecutionPayload::Node {
            subject: observation.execution.subject(),
            at: next,
            fact: TraceNodeFact::Started { call_id: None },
        };
        overlay
            .observe(
                &document,
                &ProcessObservationEventPayload::LanguageExecution(observation),
            )
            .expect("live re-entry");
        overlay.committed(&ObservedProcessEvent {
            sequence: 9,
            occurred_at_ms: 2,
            fact: ProcessLifecycleFact::EffectOutcome(
                lash_execution_contracts::process::ProcessEffectOccurrence {
                    vocabulary_version:
                        lash_execution_contracts::process::PROCESS_EVENT_VOCABULARY_VERSION,
                    origin: at.position().site().clone(),
                    at,
                    operation: "fixture".into(),
                    outcome_class: ProcessEffectOutcomeClass::Success,
                    code: None,
                    call_id: None,
                    replay_key: "fixture".into(),
                },
            ),
        });
        let snapshot = overlay.snapshot();
        let site = &snapshot.sites[0];
        assert_eq!(site.at.visit(), lash_vocabulary::sites::testkit::visit(1));
        assert_eq!(site.state, ProcessWorkflowSiteState::Running);
        assert_eq!(site.first_entered, Some(7));
        assert_eq!(site.last_settled, Some(9));
        let wire = serde_json::to_value(site).expect("site wire shape");
        assert_eq!(
            serde_json::from_value::<ProcessWorkflowSite>(wire.clone()).expect("site round trip"),
            *site
        );
        wire
    }

    #[test]
    fn live_only_sites_have_no_committed_sequences() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("process_workflow_site.fixture.json"))
                .expect("site fixture");
        assert_eq!(live_only_site_fixture(), fixture);
    }

    #[test]
    #[ignore = "regenerates crates/lash-core/src/runtime/observation/process_workflow_site.fixture.json"]
    #[expect(
        clippy::disallowed_methods,
        reason = "the explicitly invoked test-only fixture recorder owns its destination and regeneration flag"
    )]
    fn regenerate_process_workflow_site_fixture() {
        assert_eq!(std::env::var("LASH_REGENERATE").as_deref(), Ok("1"));
        let root =
            std::env::var("BUILD_WORKSPACE_DIRECTORY").expect("the recorder names its workspace");
        let path = std::path::Path::new(&root)
            .join("crates/lash-core/src/runtime/observation/process_workflow_site.fixture.json");
        std::fs::write(
            path,
            serde_json::to_string_pretty(&live_only_site_fixture()).unwrap() + "\n",
        )
        .unwrap();
    }

    /// FIG-6370: live counts are provisional until their segment's
    /// `process.ran` commits, which replaces them; a carry restarts the sum;
    /// a process not observed at `counts` reports none, never zero.
    #[test]
    fn committed_counts_replace_provisional_ones_and_restart_at_a_carry() {
        let process = ProcessId::fixture("counted-workflow");
        let document = LinkedStart {
            link: lash_vocabulary::content_ids::LinkId::from_bytes([7; 32]),
            selector: lash_vocabulary::names::StartSelector::Main,
        };
        let live = |frame, ran: Vec<(u32, u64)>| {
            let identity = TraceLanguageExecutionIdentity {
                scope: TraceRuntimeScope::none(),
                document: lash_trace::TraceExecutionCode::Start(document.clone()),
                entry_name: "main".into(),
                engine_execution_id: None,
                generation: None,
            };
            let counts =
                lash_trace::TraceCountObservation::Process(lash_trace::TraceProcessCounts {
                    process_id: process.clone(),
                    segment: "root".into(),
                    frame,
                    delta: ran,
                });
            ProcessObservationEventPayload::LanguageExecution(LanguageExecutionObservation {
                language: None,
                observed_at_ms: 100,
                execution: TraceLanguageExecution {
                    event_key: counts.event_key(&identity),
                    identity,
                    payload: TraceLanguageExecutionPayload::Counts { counts },
                },
            })
        };
        let committed = |sequence, fact| ObservedProcessEvent {
            sequence,
            occurred_at_ms: 1,
            fact,
        };
        let ran = |ran: Vec<(u32, u64)>| {
            ProcessLifecycleFact::Ran(lash_execution_contracts::process::ProcessRan::new(
                ran,
                crate::FleetFormat::current(),
            ))
        };
        let counts = |overlay: &ProcessWorkflowOverlayAccumulator| {
            overlay
                .snapshot()
                .ran
                .iter()
                .map(|ran| (ran.ordinal, ran.committed, ran.provisional))
                .collect::<Vec<_>>()
        };

        let mut overlay = ProcessWorkflowOverlayAccumulator::new(process.clone())
            .observing(ProcessWorkflowObservation::Counts);
        overlay
            .observe(&document, &live(1, vec![(3, 2)]))
            .expect("live counts");
        assert_eq!(counts(&overlay), [(3, 0, 2)]);
        overlay.committed(&committed(5, ran(vec![(3, 2), (4, 1)])));
        assert_eq!(counts(&overlay), [(3, 2, 0), (4, 1, 0)]);
        overlay
            .observe(&document, &live(2, vec![(4, 5)]))
            .expect("live counts");
        assert_eq!(counts(&overlay), [(3, 2, 0), (4, 1, 5)]);
        overlay.committed(&committed(
            6,
            ProcessLifecycleFact::Succeeded(
                lash_execution_contracts::process::ProcessSuccessorRequest {
                    payload: serde_json::Value::Null,
                    env: crate::ProcessEnvRef::new("successor"),
                    definition_id: None,
                },
            ),
        ));
        assert_eq!(counts(&overlay), []);
        overlay.committed(&committed(7, ran(vec![(1, 1)])));
        assert_eq!(counts(&overlay), [(1, 1, 0)]);
        assert_eq!(
            overlay.snapshot().observation,
            ProcessWorkflowObservation::Counts
        );

        let unobserved = ProcessWorkflowOverlayAccumulator::new(process).snapshot();
        assert_eq!(unobserved.observation, ProcessWorkflowObservation::Off);
        assert!(unobserved.ran.is_empty());
        let wire = serde_json::to_value(&unobserved).expect("overlay wire shape");
        assert_eq!(wire["observation"], "off");
        assert!(
            wire.get("ran").is_none(),
            "an unobserved overlay counts nothing: {wire}"
        );
    }

    /// FIG-6354: committed site evidence is independent of a release document.
    #[test]
    fn committed_site_states_do_not_require_a_document() {
        let process = ProcessId::fixture("committed-workflow");
        let document = LinkedStart {
            link: lash_vocabulary::content_ids::LinkId::from_bytes([7; 32]),
            selector: lash_vocabulary::names::StartSelector::Main,
        };
        let mut committed = ProcessWorkflowOverlayAccumulator::new(process.clone());
        let mut live = ProcessWorkflowOverlayAccumulator::new(process.clone());
        let at = lash_protocol_types::effect_identity_fixture("sleep", 1);
        live.observe(
            &document,
            &ProcessObservationEventPayload::LanguageExecution(LanguageExecutionObservation {
                language: None,
                observed_at_ms: 1,
                execution: TraceLanguageExecution {
                    event_key: "sleep".into(),
                    identity: TraceLanguageExecutionIdentity {
                        scope: TraceRuntimeScope::none(),
                        document: lash_trace::TraceExecutionCode::Start(document.clone()),
                        entry_name: "main".into(),
                        engine_execution_id: None,
                        generation: None,
                    },
                    payload: TraceLanguageExecutionPayload::Node {
                        subject: TraceRuntimeSubject::Process {
                            process_id: process,
                        },
                        at: at.clone(),
                        fact: TraceNodeFact::Started { call_id: None },
                    },
                },
            }),
        )
        .expect("live entry");
        let wait = lash_execution_contracts::process::WaitState {
            kind: WaitKind::Sleep { until_ms: 3 },
            since_ms: 2,
            site: Some(lash_vocabulary::sites::EffectLocation::authored(at)),
        };
        for (sequence, fact) in [
            ProcessLifecycleFact::Waiting { wait: wait.clone() },
            ProcessLifecycleFact::Resumed { wait },
        ]
        .into_iter()
        .enumerate()
        {
            let event = ObservedProcessEvent {
                sequence: sequence as u64 + 1,
                occurred_at_ms: sequence as u64 + 2,
                fact,
            };
            committed.committed(&event);
            live.committed(&event);
            assert_eq!(committed.snapshot().sites, live.snapshot().sites);
        }
        assert_eq!(live.snapshot().document, Some(document));
        let snapshot = committed.snapshot();
        assert_eq!(snapshot.sites[0].state, ProcessWorkflowSiteState::Completed);
        assert!(
            serde_json::to_value(snapshot).expect("snapshot")["document"].is_null(),
            "committed events supply no document reference"
        );
    }

    /// FIG-6354: a host's projected terminal settles sites without decoding
    /// the internal terminal record or claiming unobserved actions succeeded.
    #[test]
    fn a_stored_projected_terminal_settles_unfinished_sites() {
        let mut overlay =
            ProcessWorkflowOverlayAccumulator::new(ProcessId::fixture("projected-terminal"));
        let sleep = lash_execution_contracts::process::WaitState {
            kind: WaitKind::Sleep { until_ms: 2 },
            since_ms: 1,
            site: Some(lash_vocabulary::sites::EffectLocation::authored(
                lash_protocol_types::effect_identity_fixture("settled", 0),
            )),
        };
        let running = lash_execution_contracts::process::WaitState {
            kind: WaitKind::Process {
                process_id: ProcessId::fixture("child"),
            },
            since_ms: 3,
            site: Some(lash_vocabulary::sites::EffectLocation::authored(
                lash_protocol_types::effect_identity_fixture("running", 0),
            )),
        };
        let waiting = lash_execution_contracts::process::WaitState {
            kind: WaitKind::Sleep { until_ms: 100 },
            since_ms: 5,
            site: Some(lash_vocabulary::sites::EffectLocation::authored(
                lash_protocol_types::effect_identity_fixture("waiting", 0),
            )),
        };
        let facts = [
            ProcessLifecycleFact::Waiting {
                wait: sleep.clone(),
            },
            ProcessLifecycleFact::Resumed { wait: sleep },
            ProcessLifecycleFact::Waiting {
                wait: running.clone(),
            },
            ProcessLifecycleFact::Resumed { wait: running },
            ProcessLifecycleFact::Waiting { wait: waiting },
            ProcessLifecycleFact::Terminal {
                outcome: lash_execution_contracts::process::ProcessTerminal::from_tool_output(
                    crate::ToolCallOutput::success(serde_json::json!("done")),
                ),
                authority: None,
            },
        ];
        for (index, fact) in facts.into_iter().enumerate() {
            let event = ObservedProcessEvent {
                sequence: index as u64 + 1,
                occurred_at_ms: index as u64 + 1,
                fact,
            };
            let stored = serde_json::to_value(event.projection()).expect("host event row");
            let row: lash_execution_contracts::process::ProjectedProcessEvent =
                serde_json::from_value(stored.clone()).expect("read host row");
            if row.event_type.terminal_status().is_some() {
                assert_eq!(row.payload["await_output"]["status"], "completed");
                overlay.finish();
            } else {
                overlay.committed(&serde_json::from_value(stored).expect("typed lifecycle fact"));
            }
        }
        let snapshot = overlay.snapshot();
        assert!(snapshot.current_sites.is_empty());
        assert!(snapshot.document.is_none());
        assert_eq!(snapshot.sites.len(), 3);
        for site in snapshot.sites {
            let expected = if site.at == lash_protocol_types::effect_identity_fixture("settled", 0)
            {
                ProcessWorkflowSiteState::Completed
            } else {
                ProcessWorkflowSiteState::TerminalUnobserved
            };
            assert_eq!(site.state, expected);
        }
    }

    /// FIG-6323: process feeds also carry effects' nested language executions.
    /// Those executions have their own documents; their sites cannot replace
    /// the parent workflow's current site or contribute to its counts.
    #[test]
    fn nested_effect_executions_leave_the_process_sites_unchanged() {
        let process = ProcessId::fixture("workflow-parent");
        let document = LinkedStart {
            link: lash_vocabulary::content_ids::LinkId::from_bytes([7; 32]),
            selector: lash_vocabulary::names::StartSelector::Main,
        };
        let mut overlay = ProcessWorkflowOverlayAccumulator::new(process.clone());
        let observation = LanguageExecutionObservation {
            language: None,
            observed_at_ms: 1,
            execution: TraceLanguageExecution {
                event_key: "parent".into(),
                identity: TraceLanguageExecutionIdentity {
                    scope: TraceRuntimeScope::none(),
                    document: lash_trace::TraceExecutionCode::Start(document.clone()),
                    entry_name: "main".into(),
                    engine_execution_id: Some(process.to_string()),
                    generation: None,
                },
                payload: TraceLanguageExecutionPayload::Node {
                    subject: TraceRuntimeSubject::Process {
                        process_id: process.clone(),
                    },
                    at: lash_protocol_types::effect_identity_fixture("parent", 0),
                    fact: TraceNodeFact::Started { call_id: None },
                },
            },
        };
        let mut origin = observation.clone();
        if let TraceLanguageExecutionPayload::Node { at, fact, .. } = &mut origin.execution.payload
        {
            *fact = TraceNodeFact::Origin {
                origin: at.position().site().clone(),
            };
        }
        overlay
            .observe(
                &document,
                &ProcessObservationEventPayload::LanguageExecution(origin),
            )
            .expect("the parent entry carries its authored origin");
        overlay
            .observe(
                &document,
                &ProcessObservationEventPayload::LanguageExecution(observation.clone()),
            )
            .expect("the process's own entry");
        let before = overlay.snapshot();
        assert_eq!(before.sites.len(), 1);
        let mut nested = observation;
        nested.execution.event_key = "nested".into();
        let nested_subject = TraceRuntimeSubject::Effect {
            address: lash_protocol_types::EffectAddress::new(
                lash_protocol_types::ExecutionScope::Process {
                    process_id: process,
                },
                "nested-code",
            )
            .expect("effect address"),
            effect_id: "nested-code".into(),
        };
        let lash_trace::TraceExecutionCode::Start(start) = &mut nested.execution.identity.document
        else {
            unreachable!()
        };
        start.link = lash_vocabulary::content_ids::LinkId::from_bytes([8; 32]);
        nested.execution.payload = TraceLanguageExecutionPayload::Node {
            subject: nested_subject,
            at: lash_protocol_types::effect_identity_fixture("child", 9),
            fact: TraceNodeFact::Started { call_id: None },
        };
        overlay
            .observe(
                &document,
                &ProcessObservationEventPayload::LanguageExecution(nested),
            )
            .expect("an effect's nested execution is legal on its process feed");
        assert_eq!(overlay.snapshot(), before);
    }
    /// VIEW-ORIGIN: request attribution is keyed by effect identity, survives
    /// completion, and neither leaks to another visit nor reopens a terminal.
    #[test]
    fn request_origins_stay_with_their_occurrence_after_completion_and_reentry() {
        let process = ProcessId::fixture("origins");
        let document = LinkedStart {
            link: lash_vocabulary::content_ids::LinkId::from_bytes([3; 32]),
            selector: lash_vocabulary::names::StartSelector::Main,
        };
        let mut overlay = ProcessWorkflowOverlayAccumulator::new(process.clone());
        let at = lash_protocol_types::effect_identity_fixture("effect", 0);
        let origin = lash_vocabulary::sites::Site::new(lash_vocabulary::sites::Unit::Main, vec![4]);
        let clock = std::cell::Cell::new(100_u64);
        let observe = |overlay: &mut ProcessWorkflowOverlayAccumulator, at: &EffectId, fact| {
            overlay
                .observe(
                    &document,
                    &ProcessObservationEventPayload::LanguageExecution(
                        LanguageExecutionObservation {
                            language: None,
                            observed_at_ms: clock.get(),
                            execution: TraceLanguageExecution {
                                event_key: "origin-law".into(),
                                identity: TraceLanguageExecutionIdentity {
                                    scope: TraceRuntimeScope::none(),
                                    document: lash_trace::TraceExecutionCode::Start(
                                        document.clone(),
                                    ),
                                    entry_name: "main".into(),
                                    engine_execution_id: None,
                                    generation: None,
                                },
                                payload: TraceLanguageExecutionPayload::Node {
                                    subject: TraceRuntimeSubject::Process {
                                        process_id: process.clone(),
                                    },
                                    at: at.clone(),
                                    fact,
                                },
                            },
                        },
                    ),
                )
                .unwrap();
        };
        observe(&mut overlay, &at, TraceNodeFact::Started { call_id: None });
        observe(
            &mut overlay,
            &at,
            TraceNodeFact::Origin {
                origin: origin.clone(),
            },
        );
        observe(
            &mut overlay,
            &at,
            TraceNodeFact::Completed { call_id: None },
        );
        clock.set(200);
        observe(
            &mut overlay,
            &at,
            TraceNodeFact::Origin {
                origin: origin.clone(),
            },
        );
        assert_eq!(
            overlay.snapshot().sites[0].state,
            ProcessWorkflowSiteState::Completed
        );
        assert_eq!(overlay.snapshot().sites[0].origin, origin.clone());
        let next = lash_vocabulary::sites::testkit::with_visit(&at, 1);
        observe(
            &mut overlay,
            &next,
            TraceNodeFact::Started { call_id: None },
        );
        observe(
            &mut overlay,
            &at,
            TraceNodeFact::Origin {
                origin: origin.clone(),
            },
        );
        assert_eq!(overlay.snapshot().sites[0].origin, origin.clone());
        assert_eq!(overlay.snapshot().sites[0].at, at);
        assert_eq!(
            overlay.snapshot().sites[0].state,
            ProcessWorkflowSiteState::Completed
        );
        overlay.finish();
        observe(
            &mut overlay,
            &next,
            TraceNodeFact::Origin {
                origin: origin.clone(),
            },
        );
        let shown = overlay.snapshot();
        assert_eq!(shown.sites[0].origin, origin.clone());
        assert_eq!(
            shown.sites[0].state,
            ProcessWorkflowSiteState::TerminalUnobserved
        );
        // Attribution has no transition timestamp: an origin retained without
        // a start cannot suppress a subsequently supplied outcome.
        let mut metadata_first = ProcessWorkflowOverlayAccumulator::new(process.clone());
        clock.set(200);
        observe(
            &mut metadata_first,
            &at,
            TraceNodeFact::Origin {
                origin: origin.clone(),
            },
        );
        clock.set(50);
        observe(
            &mut metadata_first,
            &at,
            TraceNodeFact::Completed { call_id: None },
        );
        assert_eq!(
            metadata_first.snapshot().sites[0].state,
            ProcessWorkflowSiteState::Completed
        );
        let mut parked = ProcessWorkflowOverlayAccumulator::new(process.clone());
        let wait = lash_execution_contracts::process::WaitState {
            kind: WaitKind::Sleep { until_ms: 300 },
            since_ms: 100,
            site: Some(lash_vocabulary::sites::EffectLocation {
                at: at.clone(),
                origin,
            }),
        };
        parked.committed(&ObservedProcessEvent {
            sequence: 1,
            occurred_at_ms: 100,
            fact: ProcessLifecycleFact::Waiting { wait },
        });
        clock.set(200);
        observe(&mut parked, &at, TraceNodeFact::Started { call_id: None });
        assert_eq!(
            parked.snapshot().sites[0].state,
            ProcessWorkflowSiteState::Waiting {
                kind: ProcessWorkflowWaitKind::Sleep,
                resume_at_ms: Some(300),
            },
            "a delayed live start cannot reopen the committed park"
        );
    }
}
