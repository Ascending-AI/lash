//! What a site keeps of the occurrences its bounded history evicted.

use super::*;

/// The occurrences of one site at or below its watermark.
pub(super) struct SiteRetention {
    watermark: u64,
    /// What the occurrences below the watermark folded to.
    archived: WorkflowOverlaySiteState,
    archived_children: SiteChildren,
    /// The watermark occurrence, whole.
    watermark_history: OccurrenceHistory,
}

impl SiteRetention {
    /// Evict `occurrence`, the earliest the site retained, as the new
    /// watermark. The occurrence that was the watermark joins the archive.
    pub(super) fn evict(prior: Option<Self>, occurrence: u64, history: OccurrenceHistory) -> Self {
        let (archived, archived_children) = match prior {
            Some(mut prior) => {
                prior.archived.fold(
                    [(prior.watermark, &prior.watermark_history)],
                    None,
                    &mut prior.archived_children,
                );
                (prior.archived, prior.archived_children)
            }
            None => Default::default(),
        };
        Self {
            watermark: occurrence,
            archived,
            archived_children,
            watermark_history: history,
        }
    }

    pub(super) fn restore(retention: &WorkflowOverlaySiteRetention) -> Self {
        let mut watermark_history = OccurrenceHistory::default();
        for event in &retention.watermark_history {
            let observation = Observation {
                timestamp: event.timestamp,
                fact: event.fact.clone(),
            };
            if let Placement::Occurrence { transition, .. } = Placement::of(&observation) {
                watermark_history.insert(transition, observation);
            }
        }
        Self {
            watermark: retention.truncation_watermark,
            archived: retention.archived.clone(),
            archived_children: SiteChildren::of(&retention.archived_children),
            watermark_history,
        }
    }

    pub(super) fn publish(&self, site: &WorkflowTaskSite) -> WorkflowOverlaySiteRetention {
        WorkflowOverlaySiteRetention {
            site: site.clone(),
            truncation_watermark: self.watermark,
            archived: self.archived.clone(),
            archived_children: self.archived_children.iter().cloned().collect(),
            watermark_history: self.watermark_history.publish_evicted(),
        }
    }

    /// Whether `occurrence` has left the site's history.
    pub(super) fn holds(&self, occurrence: u64) -> bool {
        occurrence <= self.watermark
    }

    /// Fold an observation of an evicted occurrence. The watermark
    /// occurrence takes it exactly. Below the watermark it refines the
    /// occurrence the archive shows, and a child link is kept; anything else
    /// is of an occurrence whose history is gone, and is dropped.
    pub(super) fn observe_late(
        &mut self,
        occurrence: u64,
        transition: Transition,
        observation: Observation,
    ) {
        if occurrence == self.watermark {
            self.watermark_history.insert(transition, observation);
            return;
        }
        if let WorkflowOverlayFact::Language {
            payload:
                TraceLanguageExecutionPayload::Node {
                    fact: TraceNodeFact::ChildStarted { child },
                    ..
                },
            ..
        } = &observation.fact
        {
            self.archived_children.insert(child);
        }
        if self.archived.occurrence.occurrence() == Some(occurrence) {
            self.archived.refine(occurrence, &observation);
        }
    }

    /// The site's state through the watermark, and the children it started.
    pub(super) fn state(&self) -> (WorkflowOverlaySiteState, SiteChildren) {
        let mut state = self.archived.clone();
        let mut children = self.archived_children.clone();
        state.fold(
            [(self.watermark, &self.watermark_history)],
            None,
            &mut children,
        );
        (state, children)
    }
}

impl WorkflowOverlaySiteState {
    /// Refine the occurrence this state shows with one more observation of
    /// it, without the observations it was folded from: an earlier start
    /// moves its start, and a terminal or a branch selection ends it while
    /// it is in flight. Waits and resumes cannot be ordered against what is
    /// gone and change nothing.
    fn refine(&mut self, occurrence: u64, observation: &Observation) {
        use WorkflowOverlayOccurrence as Shown;
        let timestamp = observation.timestamp;
        let started = self.occurrence.start();
        let in_flight = matches!(
            self.occurrence,
            Shown::Running { .. } | Shown::Waiting { .. }
        );
        let fact = match &observation.fact {
            WorkflowOverlayFact::StepBodyStarted { .. } => {
                &TraceNodeFact::Started { call_id: None }
            }
            WorkflowOverlayFact::Language { payload, .. } => match payload.node_fact() {
                Some(fact) => fact,
                None => return,
            },
        };
        let ended = match fact {
            TraceNodeFact::Started { .. } => {
                let earliest = Some(started.map_or(timestamp, |start| start.min(timestamp)));
                match &mut self.occurrence {
                    Shown::Running { start, .. } => *start = (*start).min(timestamp),
                    Shown::Waiting { start, .. }
                    | Shown::Completed { start, .. }
                    | Shown::Failed { start, .. }
                    | Shown::Cancelled { start, .. } => *start = earliest,
                    Shown::Unobserved | Shown::Incomplete { .. } => return,
                }
                self.summary.started_count += u64::from(started.is_none());
                return;
            }
            TraceNodeFact::Completed { .. } => Shown::Completed {
                occurrence,
                start: started,
                end: timestamp,
            },
            TraceNodeFact::BranchSelected { selected } => {
                self.branch = Some(*selected);
                Shown::Completed {
                    occurrence,
                    start: started,
                    end: timestamp,
                }
            }
            TraceNodeFact::Failed { failure, .. } => Shown::Failed {
                occurrence,
                start: started,
                end: timestamp,
                failure: failure.clone(),
            },
            TraceNodeFact::Cancelled => Shown::Cancelled {
                occurrence,
                start: started,
                end: timestamp,
            },
            TraceNodeFact::Waiting { .. }
            | TraceNodeFact::Resumed { .. }
            | TraceNodeFact::ChildStarted { .. } => return,
        };
        if !in_flight {
            return;
        }
        let status = match ended {
            Shown::Failed { .. } => WorkflowOverlayTerminalStatus::Failed,
            Shown::Cancelled { .. } => WorkflowOverlayTerminalStatus::Cancelled,
            _ => WorkflowOverlayTerminalStatus::Completed,
        };
        self.occurrence = ended;
        self.summary.ended(WorkflowOverlayTerminalRecord {
            occurrence,
            status,
            end: timestamp,
        });
    }
}
