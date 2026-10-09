use super::*;

/// The overlay's one reducer: the canonical observations of an execution,
/// held by site and occurrence. An append touches one occurrence of one
/// site; a snapshot visits each site and each retained occurrence once.
pub struct WorkflowExecutionOverlayAccumulator {
    history_limit: usize,
    execution: Option<ExecutionRef>,
    document: Option<WorkflowOverlayDocument>,
    document_state: DocumentState,
    settlement: Option<WorkflowOverlaySettlement>,
    execution_history: ExecutionHistory,
    sites: BTreeMap<WorkflowSiteRef, SiteHistory>,
}

/// One site's occurrences above its watermark, in occurrence order, and
/// what it keeps of those at or below it.
#[derive(Default)]
struct SiteHistory {
    occurrences: BTreeMap<u64, OccurrenceHistory>,
    retention: Option<SiteRetention>,
}

impl SiteHistory {
    fn observe(
        &mut self,
        occurrence: u64,
        transition: Transition,
        observation: Observation,
        history_limit: usize,
    ) {
        match &mut self.retention {
            Some(retention) if retention.holds(occurrence) => {
                retention.observe_late(occurrence, transition, observation);
            }
            _ => {
                self.occurrences
                    .entry(occurrence)
                    .or_default()
                    .insert(transition, observation);
                self.bound(history_limit);
            }
        }
    }

    /// Evict the earliest occurrences beyond `history_limit`.
    fn bound(&mut self, history_limit: usize) {
        while self.occurrences.len() > history_limit
            && let Some((occurrence, history)) = self.occurrences.pop_first()
        {
            self.retention = Some(SiteRetention::evict(
                self.retention.take(),
                occurrence,
                history,
            ));
        }
    }

    /// The site's state and the children it started. A committed
    /// cancellation ends what was observed in flight; any other committed
    /// end leaves it incomplete instead of inventing an outcome.
    fn project(
        &self,
        settlement: Option<WorkflowOverlaySettlement>,
    ) -> (WorkflowOverlaySiteState, SiteChildren) {
        let (mut state, mut children) = match &self.retention {
            Some(retention) => retention.state(),
            None => Default::default(),
        };
        let cancelled_at = settlement.and_then(WorkflowOverlaySettlement::cancelled_at);
        if let Some(end) = cancelled_at {
            settle_retained_site(&mut state, end);
        }
        state.fold(
            self.occurrences
                .iter()
                .map(|(occurrence, history)| (*occurrence, history)),
            cancelled_at,
            &mut children,
        );
        if let Some(settlement) = settlement {
            settle_incomplete_site(&mut state, settlement);
        }
        (state, children)
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
            settlement: None,
            execution_history: Default::default(),
            sites: Default::default(),
        }
    }

    /// Continue from `previous`: the accumulator that would snapshot to it.
    /// The site index of its document is not part of an overlay; the caller
    /// supplies it again with [`Self::set_document`].
    pub(super) fn resume(previous: &WorkflowExecutionOverlay, history_limit: usize) -> Self {
        let mut accumulator = Self::new(history_limit);
        accumulator.document_state = DocumentState::of(previous);
        accumulator.settlement = previous.settlement;
        let sites = &mut accumulator.sites;
        for retention in &previous.retention {
            sites.entry(retention.site.clone()).or_default().retention =
                Some(SiteRetention::restore(retention));
        }
        for event in &previous.history {
            let observation = Observation {
                timestamp: event.timestamp,
                fact: event.fact.clone(),
            };
            match Placement::of(&observation) {
                Placement::Execution(transition) => {
                    accumulator
                        .execution_history
                        .insert(transition, observation);
                }
                Placement::Occurrence {
                    site,
                    occurrence,
                    transition,
                } => sites
                    .entry(site)
                    .or_default()
                    .occurrences
                    .entry(occurrence)
                    .or_default()
                    .insert(transition, observation),
            }
        }
        for conflict in &previous.conflicts {
            match &conflict.identity {
                WorkflowOverlayEventIdentity::Execution { transition } => accumulator
                    .execution_history
                    .restore_conflict(*transition, &conflict.variants),
                WorkflowOverlayEventIdentity::Node { at, .. }
                | WorkflowOverlayEventIdentity::StepBody { at, .. } => {
                    if let Some(history) = sites
                        .get_mut(&at.site)
                        .and_then(|site| site.occurrences.get_mut(&at.occurrence.get()))
                    {
                        history.restore_conflict(&conflict.identity, &conflict.variants);
                    }
                }
            }
        }
        for site in sites.values_mut() {
            site.bound(accumulator.history_limit);
        }
        // An overlay folded from steps alone has not been named: nothing it
        // retains came from the execution itself.
        let named = previous.generation.is_some()
            || previous.scope != TraceRuntimeScope::none()
            || previous
                .history
                .iter()
                .chain(
                    previous
                        .retention
                        .iter()
                        .flat_map(|kept| &kept.watermark_history),
                )
                .any(|event| matches!(event.fact, WorkflowOverlayFact::Language { .. }));
        accumulator.execution = Some(ExecutionRef {
            subject: previous.subject.clone(),
            name: named.then(|| ExecutionName {
                scope: previous.scope.clone(),
                generation: previous.generation,
            }),
        });
        accumulator
    }

    /// Hold the overlay to `document`, the workflow document its execution
    /// runs. What was already observed at a site the document lacks leaves
    /// the overlay as a mismatch, exactly as if the document had been here
    /// first.
    pub fn set_document(&mut self, document: WorkflowOverlayDocument) {
        let state = &mut self.document_state;
        state.loaded = Some(document.reference().clone());
        if let Some(claimed) = self.execution_history.started_document() {
            state.claim(&document, claimed);
        }
        self.sites.retain(|site, _| state.admits(&document, site));
        self.document = Some(document);
    }

    /// Whether the overlay has been given its document.
    pub fn has_document(&self) -> bool {
        self.document.is_some()
    }

    /// Fold a batch of one execution's records. A batch that spans
    /// executions is refused before changing the accumulator.
    pub fn fold(&mut self, records: &[TraceRecord]) -> Result<(), WorkflowOverlayFoldError> {
        let incoming = records.iter().filter_map(Incoming::of).collect::<Vec<_>>();
        self.admit(incoming.iter().map(|item| &item.execution))?;
        if self.execution.is_none() {
            return Err(WorkflowOverlayFoldError::NoExecutionObservations);
        }
        for item in incoming {
            self.append(item.observation);
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
        self.admit([&execution])?;
        self.append(Observation { timestamp, fact });
        Ok(())
    }

    /// Take `observed` as observations of this accumulator's execution, or
    /// refuse them all: the first one names the execution when nothing has.
    fn admit<'a>(
        &mut self,
        observed: impl IntoIterator<Item = &'a ExecutionRef>,
    ) -> Result<(), WorkflowOverlayFoldError> {
        let mut execution = self.execution.clone();
        for offered in observed {
            match &mut execution {
                None => execution = Some(offered.clone()),
                Some(held) if held.admits(offered) => held.adopt(offered),
                Some(held) => {
                    return Err(match &self.execution {
                        Some(previous) => WorkflowOverlayFoldError::PreviousExecutionMismatch {
                            previous: previous.key(),
                            event: offered.key(),
                        },
                        None => WorkflowOverlayFoldError::MixedExecutions {
                            first: held.key(),
                            other: offered.key(),
                        },
                    });
                }
            }
        }
        self.execution = execution;
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
        self.document_state.mismatches.clear();
        self.document_state.truncated = false;
    }

    fn append(&mut self, observation: Observation) {
        match Placement::of(&observation) {
            Placement::Execution(transition) => {
                if let Some(document) = &self.document
                    && let WorkflowOverlayFact::Language {
                        document: claimed,
                        payload: TraceLanguageExecutionPayload::ExecutionStarted,
                    } = &observation.fact
                {
                    self.document_state.claim(document, claimed);
                }
                self.execution_history.insert(transition, observation);
            }
            Placement::Occurrence {
                site,
                occurrence,
                transition,
            } => {
                if let Some(document) = &self.document
                    && !self.document_state.admits(document, &site)
                {
                    return;
                }
                self.sites.entry(site).or_default().observe(
                    occurrence,
                    transition,
                    observation,
                    self.history_limit,
                );
            }
        }
    }

    /// The overlay of what has been folded; `None` before any observation
    /// named an execution.
    pub fn snapshot(&self) -> Option<WorkflowExecutionOverlay> {
        use WorkflowOverlayExecutionTransition as Execution;
        let execution = self.execution.as_ref()?;
        let (mut history, mut conflicts) = (Vec::new(), Vec::new());
        self.execution_history
            .publish_execution(&mut history, &mut conflicts);
        let (mut sites, mut children, mut retention) = (Vec::new(), Vec::new(), Vec::new());
        for (site, held) in &self.sites {
            let (state, started) = held.project(self.settlement);
            children.extend(started.iter().map(|child| WorkflowOverlayChildLink {
                parent_site: site.clone(),
                child: child.clone(),
            }));
            sites.push(WorkflowOverlaySite {
                site: site.clone(),
                state,
            });
            retention.extend(held.retention.as_ref().map(|kept| kept.publish(site)));
            for observations in held.occurrences.values() {
                observations.publish_retained(&mut history, &mut conflicts);
            }
        }
        let started_document = self.execution_history.started_document();
        let status = match self.settlement {
            Some(settlement) => settlement.terminal.execution_status(),
            // A process's own finish is provisional until its actor commits
            // the terminal.
            None if is_process_subject(&execution.subject) => LanguageExecutionStatus::Running,
            None => self
                .execution_history
                .finished_status()
                .unwrap_or(LanguageExecutionStatus::Running),
        };
        Some(WorkflowExecutionOverlay {
            schema_version: TRACE_SCHEMA_VERSION,
            scope: execution
                .name
                .as_ref()
                .map_or_else(TraceRuntimeScope::none, |name| name.scope.clone()),
            subject: execution.subject.clone(),
            generation: execution.generation(),
            document: match (&self.document_state.loaded, started_document) {
                (Some(reference), _) => WorkflowOverlayDocumentBinding::Loaded {
                    reference: reference.clone(),
                },
                (None, Some(reference)) => WorkflowOverlayDocumentBinding::Claimed {
                    reference: reference.clone(),
                },
                (None, None) => WorkflowOverlayDocumentBinding::Unknown,
            },
            coverage: WorkflowOverlayCoverage {
                start_observed: self.execution_history.get(&Execution::Started).is_some(),
            },
            status,
            settlement: self.settlement,
            sites,
            children,
            mismatches: self.document_state.mismatches.iter().cloned().collect(),
            mismatches_truncated: self.document_state.truncated,
            history_limit: self.history_limit,
            retention,
            conflicts,
            history,
        })
    }
}
