//! One workflow projection for hosts: committed process facts and the live
//! observations on that process's feed share task/site/occurrence identities.
//! Session cells still use the trace overlay: they have no process event log.
use std::collections::{BTreeMap, BTreeSet};

use lash_sansio::EffectIdentity;
use lash_trace::{
    TraceLanguageExecutionPayload, TraceNodeFact, TraceRuntimeSubject, WorkflowDocumentRef,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::super::ObservedProcessEvent;
use super::process_replay::ProcessObservationEventPayload;
use crate::{ProcessEffectOutcomeClass, ProcessId, ProcessLifecycleFact, WaitKind};

/// What was observed at a site's latest occurrence. An unobserved terminal
/// does not claim the action succeeded: the process ended without that fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessWorkflowSiteState {
    Running,
    Waiting { kind: ProcessWorkflowWaitKind },
    Completed,
    Failed,
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

/// One task's latest visit to a document site. The VM's zero-based occurrence
/// index counts entries, including sleeps and calls, independently of effects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProcessWorkflowSite {
    pub at: EffectIdentity,
    pub occurrences: u64,
    pub state: ProcessWorkflowSiteState,
    /// Sequence of the first committed event naming an entry at this site,
    /// across occurrences. Live-only evidence has no sequence and leaves this
    /// `None`. Lash stores no wall time here: hosts map the sequence to the
    /// `occurred_at_ms` of the event they already hold.
    pub first_entered: Option<u64>,
    /// Sequence of the latest committed sleep resumption or effect outcome
    /// settling an occurrence at this site. Retained through later entries;
    /// live-only completion and `TerminalUnobserved` supply no settlement.
    /// Always `None` until `first_entered` is present. Lash stores no wall time
    /// here: hosts map the sequence to their event's `occurred_at_ms`.
    pub last_settled: Option<u64>,
}

/// Which provisional evidence this fold holds. RetainedWindow is bounded;
/// it never claims to be the execution's entire history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessWorkflowLiveCoverage {
    NotObserved,
    RetainedWindow,
    Gap {
        reason: super::process_replay::ProcessReplayGapReason,
    },
}

pub use lash_kernel_doc::RunObservation as ProcessWorkflowObservation;

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
    pub document: Option<WorkflowDocumentRef>,
    pub current_sites: Vec<EffectIdentity>,
    pub live_coverage: ProcessWorkflowLiveCoverage,
    pub sites: Vec<ProcessWorkflowSite>,
    pub observation: ProcessWorkflowObservation,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ran: Vec<ProcessWorkflowRan>,
}

impl ProcessWorkflowOverlay {
    /// The counts a host passes to `workflow::view::coverage`, including live
    /// provisional starts. Off remains unobserved even when the vector is empty.
    pub fn ran_counts(&self) -> lash_kernel_doc::RanCounts {
        lash_kernel_doc::RanCounts {
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
    /// A committed outcome wins over late provisional facts of that occurrence.
    durable_terminal: bool,
    timestamp: u64,
}

/// The single process workflow reducer. Feed stored events to `committed`,
/// and process-feed payloads to `observe`; neither requires diagnostic traces.
/// Replay duplicates and effect retries never increment the entry count.
pub struct ProcessWorkflowOverlayAccumulator {
    process_id: ProcessId,
    document: Option<WorkflowDocumentRef>,
    sites: BTreeMap<lash_trace::WorkflowTaskSite, Site>,
    current: BTreeMap<lash_kernel_doc::TaskIdentity, EffectIdentity>,
    live_tasks: BTreeSet<lash_kernel_doc::TaskIdentity>,
    live_entries: BTreeMap<lash_trace::WorkflowTaskSite, u64>,
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
            current: BTreeMap::new(),
            live_tasks: BTreeSet::new(),
            live_entries: BTreeMap::new(),
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
        self.live_tasks.clear();
        self.live_entries.clear();
        self.live_coverage = ProcessWorkflowLiveCoverage::Gap { reason };
    }

    fn enter_committed(&mut self, at: &EffectIdentity) {
        // Live entries are ordered by the process feed, not by wall clocks
        // on successive owners. Durable facts supply the fallback when no
        // live entry for this task survived retention.
        if !self.ended && !self.live_tasks.contains(&at.task) {
            self.current.insert(at.task.clone(), at.clone());
        }
    }

    fn enter_live(&mut self, at: &EffectIdentity) {
        if self.ended {
            return;
        }
        let key = lash_trace::WorkflowTaskSite::of(at);
        if self
            .live_entries
            .get(&key)
            .is_some_and(|held| *held >= at.occurrence)
        {
            return;
        }
        self.live_entries.insert(key, at.occurrence);
        self.live_tasks.insert(at.task.clone());
        self.current.insert(at.task.clone(), at.clone());
    }

    fn site(
        &mut self,
        at: &EffectIdentity,
        timestamp: u64,
        state: ProcessWorkflowSiteState,
        durable_terminal: bool,
        sequence: Option<u64>,
    ) {
        let key = lash_trace::WorkflowTaskSite {
            task: at.task.clone(),
            site: at.site.clone(),
        };
        let held = self.sites.entry(key).or_insert_with(|| Site {
            value: ProcessWorkflowSite {
                at: at.clone(),
                occurrences: at.occurrence.saturating_add(1),
                state: state.clone(),
                first_entered: None,
                last_settled: None,
            },
            durable_terminal,
            timestamp,
        });
        if at.occurrence > held.value.at.occurrence
            || (at.occurrence == held.value.at.occurrence
                && (durable_terminal || (!held.durable_terminal && timestamp >= held.timestamp)))
        {
            held.value.at = at.clone();
            held.value.occurrences = at.occurrence.saturating_add(1);
            held.value.state = state;
            held.durable_terminal = durable_terminal;
            held.timestamp = timestamp;
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
            ProcessLifecycleFact::Waiting { wait } | ProcessLifecycleFact::Resumed { wait } => {
                let Some(at) = &wait.site else {
                    return;
                };
                self.enter_committed(at);
                let waiting = matches!(event.fact, ProcessLifecycleFact::Waiting { .. });
                let state = if waiting {
                    ProcessWorkflowSiteState::Waiting {
                        kind: (&wait.kind).into(),
                    }
                } else if matches!(wait.kind, WaitKind::Sleep { .. }) {
                    ProcessWorkflowSiteState::Completed
                } else {
                    ProcessWorkflowSiteState::Running
                };
                self.site(
                    at,
                    now,
                    state,
                    !waiting && matches!(wait.kind, WaitKind::Sleep { .. }),
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
                self.site(&outcome.at, now, state, true, Some(event.sequence));
            }
            ProcessLifecycleFact::Ran(ran) => self.counted(&ran.ran, true),
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

    fn live_document(
        &mut self,
        document: &WorkflowDocumentRef,
    ) -> Result<(), ProcessWorkflowFoldError> {
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
        document: &WorkflowDocumentRef,
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
                if let TraceRuntimeSubject::Effect { address, .. } = &execution.identity.subject
                    && matches!(&address.execution_scope,
                        lash_sansio::ExecutionScope::Process { process_id } if process_id == self.process_id)
                {
                    return Ok(());
                }
                if &execution.identity.document != document
                    || !matches!(&execution.identity.subject,
                    TraceRuntimeSubject::Process { process_id } if process_id == self.process_id)
                {
                    return Err(ProcessWorkflowFoldError::DifferentExecution);
                }
                self.live_document(document)?;
                self.live_coverage = ProcessWorkflowLiveCoverage::RetainedWindow;
                if let TraceLanguageExecutionPayload::Ran { ran, .. } = &execution.payload
                    && !self.ended
                {
                    self.counted(ran, false);
                }
                if let TraceLanguageExecutionPayload::Node { at, fact } = &execution.payload {
                    let state = match fact {
                        TraceNodeFact::Started { .. } => {
                            self.enter_live(at);
                            ProcessWorkflowSiteState::Running
                        }
                        TraceNodeFact::Completed { .. } => ProcessWorkflowSiteState::Completed,
                        TraceNodeFact::Failed { .. } => ProcessWorkflowSiteState::Failed,
                        TraceNodeFact::Cancelled => ProcessWorkflowSiteState::Cancelled,
                        TraceNodeFact::Waiting { awaited } => ProcessWorkflowSiteState::Waiting {
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
                    self.site(at, observation.observed_at_ms, state, false, None);
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
        TraceRuntimeScope, WorkflowDocumentEntry,
    };

    /// FIG-6358: committed entry and settlement sequences survive re-entry,
    /// duplicate replay and reversed wall clocks.
    #[test]
    fn committed_site_sequences_retain_first_entry_and_latest_settlement() {
        let mut overlay =
            ProcessWorkflowOverlayAccumulator::new(ProcessId::fixture("site-sequences"));
        let mut at = lash_sansio::effect_identity_fixture("sleep", 0);
        for (sequence, occurrence, waiting, first_entered, last_settled) in [
            (7, 0, true, 7, None),
            (11, 0, false, 7, Some(11)),
            (15, 1, true, 7, Some(11)),
            (19, 1, false, 7, Some(19)),
            (19, 1, false, 7, Some(19)),
        ] {
            at.occurrence = occurrence;
            let wait = crate::WaitState {
                kind: WaitKind::Sleep { until_ms: 100 },
                since_ms: 1,
                site: Some(at.clone()),
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
    #[test]
    fn live_only_sites_have_no_committed_sequences() {
        let process = ProcessId::fixture("live-site-sequences");
        let document = WorkflowDocumentRef {
            document: lash_kernel_doc::DocumentId::from_bytes([7; 32]),
            entry: WorkflowDocumentEntry::Main,
        };
        let mut overlay = ProcessWorkflowOverlayAccumulator::new(process.clone());
        let at = lash_sansio::effect_identity_fixture("effect", 0);
        let mut observation = LanguageExecutionObservation {
            language: None,
            observed_at_ms: 100,
            execution: TraceLanguageExecution {
                event_key: "live-site".into(),
                identity: TraceLanguageExecutionIdentity {
                    scope: TraceRuntimeScope::none(),
                    subject: TraceRuntimeSubject::Process {
                        process_id: process,
                    },
                    document: document.clone(),
                    entry_name: "main".into(),
                    engine_execution_id: None,
                    generation: None,
                },
                payload: TraceLanguageExecutionPayload::Node {
                    at: at.clone(),
                    fact: TraceNodeFact::Started { call_id: None },
                },
            },
        };
        for fact in [
            TraceNodeFact::Started { call_id: None },
            TraceNodeFact::Cancelled,
        ] {
            observation.execution.payload = TraceLanguageExecutionPayload::Node {
                at: at.clone(),
                fact,
            };
            overlay
                .observe(
                    &document,
                    &ProcessObservationEventPayload::LanguageExecution(observation.clone()),
                )
                .expect("live evidence");
            let snapshot = serde_json::to_value(overlay.snapshot()).expect("overlay wire shape");
            let site = snapshot["sites"][0].as_object().expect("site");
            for field in ["first_entered", "last_settled"] {
                assert_eq!(site.get(field), Some(&serde_json::Value::Null));
            }
        }
        overlay.committed(&ObservedProcessEvent {
            sequence: 7,
            occurred_at_ms: 1,
            fact: ProcessLifecycleFact::EffectOutcome(crate::ProcessEffectOccurrence {
                vocabulary_version: crate::PROCESS_EVENT_VOCABULARY_VERSION,
                at: at.clone(),
                operation: "fixture".into(),
                outcome_class: ProcessEffectOutcomeClass::Failure,
                code: None,
                call_id: None,
                replay_key: "fixture".into(),
            }),
        });
        let snapshot = overlay.snapshot();
        assert_eq!(snapshot.sites[0].first_entered, Some(7));
        assert_eq!(snapshot.sites[0].last_settled, Some(7));

        // A later live occurrence owns the displayed state before the prior
        // occurrence's next committed settlement reaches this consumer.
        let mut next = at.clone();
        next.occurrence = 1;
        observation.execution.payload = TraceLanguageExecutionPayload::Node {
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
            fact: ProcessLifecycleFact::EffectOutcome(crate::ProcessEffectOccurrence {
                vocabulary_version: crate::PROCESS_EVENT_VOCABULARY_VERSION,
                at,
                operation: "fixture".into(),
                outcome_class: ProcessEffectOutcomeClass::Success,
                code: None,
                call_id: None,
                replay_key: "fixture".into(),
            }),
        });
        let snapshot = overlay.snapshot();
        let site = &snapshot.sites[0];
        assert_eq!(site.at.occurrence, 1);
        assert_eq!(site.state, ProcessWorkflowSiteState::Running);
        assert_eq!(site.first_entered, Some(7));
        assert_eq!(site.last_settled, Some(9));
        let wire = serde_json::to_value(site).expect("site wire shape");
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("process_workflow_site.fixture.json"))
                .expect("site fixture");
        assert_eq!(wire, fixture);
        assert_eq!(
            serde_json::from_value::<ProcessWorkflowSite>(wire).expect("site round trip"),
            *site
        );
    }

    /// FIG-6370: live counts are provisional until their segment's
    /// `process.ran` commits, which replaces them; a carry restarts the sum;
    /// a process not observed at `counts` reports none, never zero.
    #[test]
    fn committed_counts_replace_provisional_ones_and_restart_at_a_carry() {
        let process = ProcessId::fixture("counted-workflow");
        let document = WorkflowDocumentRef {
            document: lash_kernel_doc::DocumentId::from_bytes([7; 32]),
            entry: WorkflowDocumentEntry::Main,
        };
        let live = |ran: Vec<(u32, u64)>| {
            ProcessObservationEventPayload::LanguageExecution(LanguageExecutionObservation {
                language: None,
                observed_at_ms: 100,
                execution: TraceLanguageExecution {
                    event_key: "ran".into(),
                    identity: TraceLanguageExecutionIdentity {
                        scope: TraceRuntimeScope::none(),
                        subject: TraceRuntimeSubject::Process {
                            process_id: process.clone(),
                        },
                        document: document.clone(),
                        entry_name: "main".into(),
                        engine_execution_id: None,
                        generation: None,
                    },
                    payload: TraceLanguageExecutionPayload::Ran { ran, segment: None },
                },
            })
        };
        let committed = |sequence, fact| ObservedProcessEvent {
            sequence,
            occurred_at_ms: 1,
            fact,
        };
        let ran = |ran: Vec<(u32, u64)>| {
            ProcessLifecycleFact::Ran(crate::ProcessRan::new(ran, crate::FleetFormat::current()))
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
            .observe(&document, &live(vec![(3, 2)]))
            .expect("live counts");
        assert_eq!(counts(&overlay), [(3, 0, 2)]);
        overlay.committed(&committed(5, ran(vec![(3, 2), (4, 1)])));
        assert_eq!(counts(&overlay), [(3, 2, 0), (4, 1, 0)]);
        overlay
            .observe(&document, &live(vec![(4, 5)]))
            .expect("live counts");
        assert_eq!(counts(&overlay), [(3, 2, 0), (4, 1, 5)]);
        overlay.committed(&committed(
            6,
            ProcessLifecycleFact::Succeeded(crate::ProcessSuccessorRequest {
                payload: serde_json::Value::Null,
                env: crate::ProcessEnvRef::new("successor"),
                definition_id: None,
            }),
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
        let document = WorkflowDocumentRef {
            document: lash_kernel_doc::DocumentId::from_bytes([7; 32]),
            entry: WorkflowDocumentEntry::Main,
        };
        let mut committed = ProcessWorkflowOverlayAccumulator::new(process.clone());
        let mut live = ProcessWorkflowOverlayAccumulator::new(process.clone());
        let at = lash_sansio::effect_identity_fixture("sleep", 1);
        live.observe(
            &document,
            &ProcessObservationEventPayload::LanguageExecution(LanguageExecutionObservation {
                language: None,
                observed_at_ms: 1,
                execution: TraceLanguageExecution {
                    event_key: "sleep".into(),
                    identity: TraceLanguageExecutionIdentity {
                        scope: TraceRuntimeScope::none(),
                        subject: TraceRuntimeSubject::Process {
                            process_id: process,
                        },
                        document: document.clone(),
                        entry_name: "main".into(),
                        engine_execution_id: None,
                        generation: None,
                    },
                    payload: TraceLanguageExecutionPayload::Node {
                        at: at.clone(),
                        fact: TraceNodeFact::Started { call_id: None },
                    },
                },
            }),
        )
        .expect("live entry");
        let wait = crate::WaitState {
            kind: WaitKind::Sleep { until_ms: 3 },
            since_ms: 2,
            site: Some(at),
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
        let sleep = crate::WaitState {
            kind: WaitKind::Sleep { until_ms: 2 },
            since_ms: 1,
            site: Some(lash_sansio::effect_identity_fixture("settled", 0)),
        };
        let running = crate::WaitState {
            kind: WaitKind::Process {
                process_id: ProcessId::fixture("child"),
            },
            since_ms: 3,
            site: Some(lash_sansio::effect_identity_fixture("running", 0)),
        };
        let waiting = crate::WaitState {
            kind: WaitKind::Sleep { until_ms: 100 },
            since_ms: 5,
            site: Some(lash_sansio::effect_identity_fixture("waiting", 0)),
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
                outcome: crate::ProcessTerminal::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::json!("done"),
                )),
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
            let row: crate::ProjectedProcessEvent =
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
            let expected = if site.at == lash_sansio::effect_identity_fixture("settled", 0) {
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
        let document = WorkflowDocumentRef {
            document: lash_kernel_doc::DocumentId::from_bytes([7; 32]),
            entry: WorkflowDocumentEntry::Main,
        };
        let mut overlay = ProcessWorkflowOverlayAccumulator::new(process.clone());
        let observation = LanguageExecutionObservation {
            language: None,
            observed_at_ms: 1,
            execution: TraceLanguageExecution {
                event_key: "parent".into(),
                identity: TraceLanguageExecutionIdentity {
                    scope: TraceRuntimeScope::none(),
                    subject: TraceRuntimeSubject::Process {
                        process_id: process.clone(),
                    },
                    document: document.clone(),
                    entry_name: "main".into(),
                    engine_execution_id: Some(process.to_string()),
                    generation: None,
                },
                payload: TraceLanguageExecutionPayload::Node {
                    at: lash_sansio::effect_identity_fixture("parent", 0),
                    fact: TraceNodeFact::Started { call_id: None },
                },
            },
        };
        overlay
            .observe(
                &document,
                &ProcessObservationEventPayload::LanguageExecution(observation.clone()),
            )
            .expect("the process's own entry");
        let before = overlay.snapshot();
        let mut nested = observation;
        nested.execution.event_key = "nested".into();
        nested.execution.identity.subject = TraceRuntimeSubject::Effect {
            address: lash_sansio::EffectAddress::new(
                lash_sansio::ExecutionScope::Process {
                    process_id: process,
                },
                "nested-code",
            )
            .expect("effect address"),
            effect_id: "nested-code".into(),
        };
        nested.execution.identity.document.document =
            lash_kernel_doc::DocumentId::from_bytes([8; 32]);
        nested.execution.payload = TraceLanguageExecutionPayload::Node {
            at: lash_sansio::effect_identity_fixture("child", 9),
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
}
