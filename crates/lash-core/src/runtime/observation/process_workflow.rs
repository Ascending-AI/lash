//! One workflow projection for hosts: committed process facts and the live
//! observations on that process's feed share task/site/occurrence identities.
//! Session cells still use the trace overlay: they have no process event log.
use std::collections::{BTreeMap, BTreeSet};

use lash_sansio::EffectIdentity;
use lash_trace::{
    TraceLanguageExecutionPayload, TraceNodeFact, TraceRuntimeSubject, WorkflowDocumentRef,
};
use serde::{Deserialize, Serialize};

use super::super::ObservedProcessEvent;
use super::process_replay::ProcessObservationEventPayload;
use crate::{ProcessEffectOutcomeClass, ProcessId, ProcessLifecycleFact, WaitKind};

/// What was observed at a site's latest occurrence. An unobserved terminal
/// does not claim the action succeeded: the process ended without that fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessWorkflowSite {
    pub at: EffectIdentity,
    pub occurrences: u64,
    pub state: ProcessWorkflowSiteState,
}

/// Which provisional evidence this fold holds. RetainedWindow is bounded;
/// it never claims to be the execution's entire history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessWorkflowLiveCoverage {
    NotObserved,
    RetainedWindow,
    Gap {
        reason: super::process_replay::ProcessReplayGapReason,
    },
}

/// The workflow part of a process observation. Durable coverage is the
/// enclosing snapshot's `effects.coverage`; live entries are provisional and
/// can be lost at a replay gap. `current_sites` retains the last entered site
/// of each task through pure computation and is empty once the process ends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessWorkflowOverlay {
    pub process_id: ProcessId,
    pub document: WorkflowDocumentRef,
    pub current_sites: Vec<EffectIdentity>,
    pub live_coverage: ProcessWorkflowLiveCoverage,
    pub sites: Vec<ProcessWorkflowSite>,
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
    document: WorkflowDocumentRef,
    sites: BTreeMap<lash_trace::WorkflowTaskSite, Site>,
    current: BTreeMap<lash_kernel_doc::TaskIdentity, EffectIdentity>,
    live_tasks: BTreeSet<lash_kernel_doc::TaskIdentity>,
    live_entries: BTreeMap<lash_trace::WorkflowTaskSite, u64>,
    through: u64,
    ended: bool,
    live_coverage: ProcessWorkflowLiveCoverage,
}

impl ProcessWorkflowOverlayAccumulator {
    pub fn new(process_id: ProcessId, document: WorkflowDocumentRef) -> Self {
        Self {
            process_id,
            document,
            sites: BTreeMap::new(),
            current: BTreeMap::new(),
            live_tasks: BTreeSet::new(),
            live_entries: BTreeMap::new(),
            through: 0,
            ended: false,
            live_coverage: ProcessWorkflowLiveCoverage::NotObserved,
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
            },
            durable_terminal,
            timestamp,
        });
        if at.occurrence > held.value.at.occurrence
            || (at.occurrence == held.value.at.occurrence
                && (durable_terminal || (!held.durable_terminal && timestamp >= held.timestamp)))
        {
            held.value = ProcessWorkflowSite {
                at: at.clone(),
                occurrences: at.occurrence.saturating_add(1),
                state,
            };
            held.durable_terminal = durable_terminal;
            held.timestamp = timestamp;
        }
    }

    /// A host's stored, typed lifecycle event, in sequence order. An already
    /// folded sequence is ignored, including commits replayed after a snapshot.
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
                );
            }
            ProcessLifecycleFact::EffectOutcome(outcome) => {
                self.enter_committed(&outcome.at);
                let state = match outcome.outcome_class {
                    ProcessEffectOutcomeClass::Success => ProcessWorkflowSiteState::Completed,
                    ProcessEffectOutcomeClass::Failure => ProcessWorkflowSiteState::Failed,
                    ProcessEffectOutcomeClass::Cancelled => ProcessWorkflowSiteState::Cancelled,
                };
                self.site(&outcome.at, now, state, true);
            }
            ProcessLifecycleFact::Terminal { .. } => self.finish(),
            _ => {}
        }
    }

    pub fn finish(&mut self) {
        self.ended = true;
        self.current.clear();
        for site in self.sites.values_mut() {
            if matches!(
                site.value.state,
                ProcessWorkflowSiteState::Running | ProcessWorkflowSiteState::Waiting { .. }
            ) {
                site.value.state = ProcessWorkflowSiteState::TerminalUnobserved;
            }
        }
    }

    /// Fold exactly what a process feed emits. The process's committed
    /// terminal stays authoritative when delayed live observations arrive.
    pub fn observe(
        &mut self,
        event: &ProcessObservationEventPayload,
    ) -> Result<(), ProcessWorkflowFoldError> {
        match event {
            ProcessObservationEventPayload::Committed { event } => self.committed(event),
            ProcessObservationEventPayload::StepBodyStarted(observation) => {
                if observation.step.process_id != self.process_id {
                    return Err(ProcessWorkflowFoldError::DifferentExecution);
                }
                self.live_coverage = ProcessWorkflowLiveCoverage::RetainedWindow;
                self.enter_live(&observation.step.at);
                self.site(
                    &observation.step.at,
                    observation.observed_at_ms,
                    ProcessWorkflowSiteState::Running,
                    false,
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
                if execution.identity.document != self.document
                    || !matches!(&execution.identity.subject,
                    TraceRuntimeSubject::Process { process_id } if process_id == self.process_id)
                {
                    return Err(ProcessWorkflowFoldError::DifferentExecution);
                }
                self.live_coverage = ProcessWorkflowLiveCoverage::RetainedWindow;
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
                    self.site(at, observation.observed_at_ms, state, false);
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
        let mut overlay = ProcessWorkflowOverlayAccumulator::new(process.clone(), document.clone());
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
                    document,
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
            .observe(&ProcessObservationEventPayload::LanguageExecution(
                observation.clone(),
            ))
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
            .observe(&ProcessObservationEventPayload::LanguageExecution(nested))
            .expect("an effect's nested execution is legal on its process feed");
        assert_eq!(overlay.snapshot(), before);
    }
}
