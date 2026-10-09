use std::collections::HashMap;

use super::*;

/// Mutable canonical history. Appends touch one site's bounded occurrence
/// index; snapshot reads pay for sorting and projecting the retained
/// observations.
pub struct WorkflowExecutionOverlayAccumulator {
    history_limit: usize,
    execution: Option<ExecutionRef>,
    document: Option<WorkflowOverlayDocument>,
    document_state: DocumentState,
    status: Option<LanguageExecutionStatus>,
    settlement: Option<WorkflowOverlaySettlement>,
    execution_history: BTreeMap<WorkflowOverlayEventIdentity, HistoryEntry>,
    sites: HashMap<WorkflowSiteRef, SiteHistory>,
}

#[derive(Default)]
struct SiteHistory {
    occurrences: BTreeMap<u64, BTreeMap<WorkflowOverlayEventIdentity, HistoryEntry>>,
    retention: Option<WorkflowOverlaySiteRetention>,
}

struct HistoryEntry {
    event: WorkflowOverlayHistoryEvent,
    variants: BTreeSet<String>,
}

fn insert_event(
    history: &mut BTreeMap<WorkflowOverlayEventIdentity, HistoryEntry>,
    candidate: WorkflowOverlayHistoryEvent,
) {
    match history.entry(candidate.identity.clone()) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(HistoryEntry {
                event: candidate,
                variants: BTreeSet::new(),
            });
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            let current = entry.get_mut();
            if current.event != candidate {
                insert_bounded_variant(&mut current.variants, history_digest(&current.event));
                insert_bounded_variant(&mut current.variants, history_digest(&candidate));
                current.event = canonical_history_event(current.event.clone(), candidate);
            }
        }
    }
}

impl Default for WorkflowExecutionOverlayAccumulator {
    fn default() -> Self {
        Self::new(DEFAULT_WORKFLOW_OVERLAY_HISTORY_LIMIT)
    }
}

impl WorkflowExecutionOverlayAccumulator {
    /// Retain at most `history_limit` occurrences per site (at least one).
    /// The standard preset is 256; no workload measurements justify that value.
    pub fn new(history_limit: usize) -> Self {
        Self {
            history_limit: history_limit.max(1),
            execution: None,
            document: None,
            document_state: DocumentState::default(),
            status: None,
            settlement: None,
            execution_history: Default::default(),
            sites: Default::default(),
        }
    }

    /// Hold the overlay to `document`, the workflow document its execution
    /// runs. What was already observed at a site the document lacks leaves
    /// the overlay as a mismatch, exactly as if the document had been here
    /// first.
    pub fn set_document(&mut self, document: WorkflowOverlayDocument) {
        self.document_state.loaded = Some(document.reference().clone());
        let state = &mut self.document_state;
        for entry in self.execution_history.values() {
            state.admit(Some(&document), &entry.event.identity, &entry.event.fact);
        }
        self.sites.retain(|site, _| {
            let known = document.contains(site);
            if !known {
                state.report(WorkflowOverlayMismatch::SiteOutsideDocument { site: site.clone() });
            }
            known
        });
        self.document = Some(document);
    }

    /// Whether the overlay has been given its document.
    pub fn has_document(&self) -> bool {
        self.document.is_some()
    }

    /// Fold a batch into one overlay without cloning its retained history.
    /// A batch that spans executions is refused before changing the
    /// accumulator.
    pub fn fold(&mut self, records: &[TraceRecord]) -> Result<(), WorkflowOverlayFoldError> {
        let incoming = records.iter().filter_map(Incoming::of).collect::<Vec<_>>();
        let Some((execution_key, subject)) = batch_execution(self.execution.as_ref(), &incoming)
        else {
            return Err(WorkflowOverlayFoldError::NoExecutionObservations);
        };
        for item in &incoming {
            if !item.belongs_to(&execution_key, &subject) {
                let other = item.execution.key();
                return Err(match &self.execution {
                    Some(_) => WorkflowOverlayFoldError::PreviousExecutionMismatch {
                        previous: execution_key,
                        event: other,
                    },
                    None => WorkflowOverlayFoldError::MixedExecutions {
                        first: execution_key,
                        other,
                    },
                });
            }
        }
        for item in incoming {
            self.append(item);
        }
        Ok(())
    }

    /// Fold a canonical language observation without a diagnostic trace envelope.
    pub fn observe(
        &mut self,
        observation: &crate::LanguageExecutionObservation,
    ) -> Result<(), WorkflowOverlayFoldError> {
        self.observe_fact(
            observation.observed_at_ms,
            ExecutionRef::of_language(&observation.execution.identity),
            WorkflowOverlayFact::Language {
                document: Box::new(observation.execution.identity.document.clone()),
                payload: observation.execution.payload.clone(),
            },
        )
    }

    /// Fold the start of an admitted step body: it binds the step's site
    /// occurrence to its call.
    pub fn step_body_started(
        &mut self,
        observation: &crate::StepBodyStartedObservation,
    ) -> Result<(), WorkflowOverlayFoldError> {
        self.observe_fact(
            observation.observed_at_ms,
            ExecutionRef::of_step(&observation.step),
            WorkflowOverlayFact::StepBodyStarted {
                step: observation.step.clone(),
            },
        )
    }

    fn observe_fact(
        &mut self,
        observed_at_ms: u64,
        execution: ExecutionRef,
        fact: WorkflowOverlayFact,
    ) -> Result<(), WorkflowOverlayFoldError> {
        let timestamp = i64::try_from(observed_at_ms)
            .ok()
            .and_then(DateTime::from_timestamp_millis)
            .ok_or(WorkflowOverlayFoldError::InvalidObservationTimestamp { observed_at_ms })?;
        let incoming = Incoming {
            timestamp,
            execution,
            fact,
        };
        if let Some(previous) = &self.execution
            && !incoming.belongs_to(&previous.key(), &previous.subject)
        {
            return Err(WorkflowOverlayFoldError::PreviousExecutionMismatch {
                previous: previous.key(),
                event: incoming.execution.key(),
            });
        }
        self.append(incoming);
        Ok(())
    }

    /// Reconcile committed or snapshot terminal evidence, even before replay arrives.
    pub fn settle(&mut self, settlement: WorkflowOverlaySettlement) {
        self.settlement = Some(settlement.refine(self.settlement));
    }

    /// Discard provisional continuity after a gap or reexecution boundary.
    /// The document and durable terminal authority survive the reset.
    pub fn reset_live(&mut self) {
        self.execution_history.clear();
        self.sites.clear();
        self.status = None;
        self.document_state.mismatches.clear();
        self.document_state.truncated = false;
    }

    fn append(&mut self, incoming: Incoming) {
        let Incoming {
            timestamp,
            execution,
            fact,
        } = incoming;
        let execution = match self.execution.take() {
            Some(previous) => previous.canonical(execution),
            None => execution,
        };
        let status = self.status.unwrap_or(LanguageExecutionStatus::Running);
        self.status = Some(observed_execution_status(status, &execution.subject, &fact));
        let identity = event_identity(&fact);
        if !self
            .document_state
            .admit(self.document.as_ref(), &identity, &fact)
        {
            self.execution = Some(execution);
            return;
        }
        if let Some((site, occurrence)) = site_occurrence(&identity) {
            let site = site.clone();
            let history = self.sites.entry(site.clone()).or_default();
            if let Some(retention) = &mut history.retention
                && occurrence <= retention.truncation_watermark
            {
                merge_late_retained_event(retention, timestamp, &fact, &execution);
            } else {
                insert_event(
                    history.occurrences.entry(occurrence).or_default(),
                    WorkflowOverlayHistoryEvent {
                        identity,
                        timestamp,
                        fact,
                    },
                );
                if history.occurrences.len() > self.history_limit
                    && let Some((occurrence, dropped)) = history.occurrences.pop_first()
                {
                    let dropped: Vec<_> = dropped.into_values().map(|entry| entry.event).collect();
                    history.retention = Some(merge_site_retention(
                        history.retention.take(),
                        &site,
                        occurrence,
                        &dropped,
                        &execution,
                    ));
                }
            }
        } else {
            insert_event(
                &mut self.execution_history,
                WorkflowOverlayHistoryEvent {
                    identity,
                    timestamp,
                    fact,
                },
            );
        }
        self.execution = Some(execution);
    }

    /// Materialize the pure fold's canonical overlay.
    pub fn snapshot(&self) -> Option<WorkflowExecutionOverlay> {
        let execution = self.execution.as_ref()?;
        let entries = self.execution_history.values().chain(
            self.sites
                .values()
                .flat_map(|site| site.occurrences.values().flat_map(BTreeMap::values)),
        );
        let mut history = Vec::new();
        let mut conflicts = Vec::new();
        for entry in entries {
            history.push(entry.event.clone());
            if !entry.variants.is_empty() {
                conflicts.push(WorkflowOverlayConflict {
                    identity: entry.event.identity.clone(),
                    kind: WorkflowOverlayConflictKind::ConflictingDuplicate,
                    variants: entry.variants.iter().cloned().collect(),
                });
            }
        }
        history.sort_by(|left, right| left.identity.cmp(&right.identity));
        conflicts.sort_by(|left, right| left.identity.cmp(&right.identity));
        let mut retention: Vec<_> = self
            .sites
            .values()
            .filter_map(|site| site.retention.clone())
            .collect();
        retention.sort_by(|left, right| left.site.cmp(&right.site));
        Some(materialize_overlay(
            execution.clone(),
            self.document_state.clone(),
            history,
            conflicts,
            self.history_limit,
            retention,
            ExecutionProjection {
                status: self.status.unwrap_or(LanguageExecutionStatus::Running),
                settlement: self.settlement,
            },
        ))
    }
}
