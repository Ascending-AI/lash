//! The execution overlay of a workflow document: a pure, bounded,
//! deterministic fold of what one execution was observed to do, keyed by the
//! document's execution sites.
//!
//! The reducer reads three things: the immutable document (as the site index
//! [`WorkflowOverlayDocument`]), the provisional observations of the
//! execution, and the committed settlement of its process. It copies nothing
//! static out of the document.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use lash_sansio::WorkflowSiteRef;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    StepBodyStarted, TRACE_SCHEMA_VERSION, TraceEvent, TraceLanguageExecutionFailure,
    TraceLanguageExecutionGeneration, TraceLanguageExecutionIdentity as LanguageIdentity,
    TraceLanguageExecutionPayload, TraceLanguageExecutionStatus as LanguageExecutionStatus,
    TraceRecord, TraceRuntimeScope, TraceRuntimeSubject, WorkflowDocumentRef,
};

mod accumulator;
mod model;
mod settlement;
pub use accumulator::WorkflowExecutionOverlayAccumulator;
pub use model::*;
use settlement::{
    canonical_execution_status_of, observed_execution_status, settle_incomplete_sites,
    settle_retained_sites,
};

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WorkflowOverlayFoldError {
    #[error("observation timestamp {observed_at_ms}ms is out of range")]
    InvalidObservationTimestamp { observed_at_ms: u64 },
    #[error("the fold requires at least one execution observation")]
    NoExecutionObservations,
    #[error("the fold input spans executions `{first}` and `{other}`")]
    MixedExecutions { first: String, other: String },
    #[error("the previous overlay is for `{previous}`, not `{event}`")]
    PreviousExecutionMismatch { previous: String, event: String },
    #[error("history limit must be greater than zero")]
    ZeroHistoryLimit,
}

/// The execution an overlay is of: who ran, and which attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ExecutionRef {
    scope: TraceRuntimeScope,
    subject: TraceRuntimeSubject,
    generation: Option<TraceLanguageExecutionGeneration>,
}

impl ExecutionRef {
    fn of(overlay: &WorkflowExecutionOverlay) -> Self {
        Self {
            scope: overlay.scope.clone(),
            subject: overlay.subject.clone(),
            generation: overlay.generation,
        }
    }

    fn of_language(identity: &LanguageIdentity) -> Self {
        Self {
            scope: identity.scope.clone(),
            subject: identity.subject.clone(),
            generation: identity.generation,
        }
    }

    /// The execution a process step belongs to: the process's own.
    fn of_step(step: &StepBodyStarted) -> Self {
        Self {
            scope: TraceRuntimeScope::none(),
            subject: TraceRuntimeSubject::Process {
                process_id: step.process_id.clone(),
            },
            generation: None,
        }
    }

    fn key(&self) -> String {
        match self.generation {
            Some(generation) => format!(
                "{}:attempt:{attempt}",
                self.subject.graph_key(),
                attempt = generation.attempt(),
            ),
            None => self.subject.graph_key(),
        }
    }

    fn canonical(self, other: Self) -> Self {
        Self {
            scope: canonical_value(self.scope, other.scope),
            subject: canonical_value(self.subject, other.subject),
            generation: canonical_value(self.generation, other.generation),
        }
    }
}

/// One observation as the reducer takes it in.
struct Incoming {
    timestamp: DateTime<Utc>,
    execution: ExecutionRef,
    fact: WorkflowOverlayFact,
}

impl Incoming {
    fn of(record: &TraceRecord) -> Option<Self> {
        match &record.event {
            TraceEvent::LanguageExecution { event, .. } => Some(Self {
                timestamp: record.timestamp,
                execution: ExecutionRef::of_language(&event.identity),
                fact: WorkflowOverlayFact::Language {
                    document: Box::new(event.identity.document.clone()),
                    payload: event.payload.clone(),
                },
            }),
            TraceEvent::StepBodyStarted { step } => Some(Self {
                timestamp: record.timestamp,
                execution: ExecutionRef::of_step(step),
                fact: WorkflowOverlayFact::StepBodyStarted { step: step.clone() },
            }),
            _ => None,
        }
    }

    /// Whether this observation is of the execution `key` names, whose
    /// subject is `subject`. A step names only its process: it belongs to
    /// that process's execution whatever its key.
    fn belongs_to(&self, key: &str, subject: &TraceRuntimeSubject) -> bool {
        match &self.fact {
            WorkflowOverlayFact::Language { .. } => self.execution.key() == key,
            WorkflowOverlayFact::StepBodyStarted { .. } => self.execution.subject == *subject,
        }
    }
}

/// The execution a batch is of: the previous overlay's, else the first
/// language observation's, else the first step's process.
fn batch_execution(
    previous: Option<&ExecutionRef>,
    incoming: &[Incoming],
) -> Option<(String, TraceRuntimeSubject)> {
    let named = previous.or_else(|| {
        incoming
            .iter()
            .find(|item| matches!(item.fact, WorkflowOverlayFact::Language { .. }))
            .or(incoming.first())
            .map(|item| &item.execution)
    })?;
    Some((named.key(), named.subject.clone()))
}

/// What the reducer knows of the document, and what did not belong to it.
#[derive(Clone, Debug, Default)]
struct DocumentState {
    loaded: Option<WorkflowDocumentRef>,
    mismatches: BTreeSet<WorkflowOverlayMismatch>,
    truncated: bool,
}

impl DocumentState {
    fn of(overlay: &WorkflowExecutionOverlay) -> Self {
        Self {
            loaded: overlay
                .coverage
                .document_loaded
                .then(|| overlay.document.clone())
                .flatten(),
            mismatches: overlay.mismatches.iter().cloned().collect(),
            truncated: overlay.mismatches_truncated,
        }
    }

    /// Keep the smallest mismatches: which are kept does not depend on the
    /// order they were reported in.
    fn report(&mut self, mismatch: WorkflowOverlayMismatch) {
        self.mismatches.insert(mismatch);
        while self.mismatches.len() > MISMATCH_LIMIT {
            self.mismatches.pop_last();
            self.truncated = true;
        }
    }

    /// Whether `fact` belongs to `document`. One that does not is reported.
    /// A start that names another document is reported and kept: the
    /// execution did start.
    fn admit(
        &mut self,
        document: Option<&WorkflowOverlayDocument>,
        identity: &WorkflowOverlayEventIdentity,
        fact: &WorkflowOverlayFact,
    ) -> bool {
        let Some(document) = document else {
            return true;
        };
        if let WorkflowOverlayFact::Language {
            document: claimed,
            payload: TraceLanguageExecutionPayload::ExecutionStarted,
            ..
        } = fact
            && claimed.as_ref() != document.reference()
        {
            self.report(WorkflowOverlayMismatch::Document {
                claimed: claimed.as_ref().clone(),
            });
        }
        match &identity.site {
            Some(site) if !document.contains(site) => {
                self.report(WorkflowOverlayMismatch::SiteOutsideDocument { site: site.clone() });
                false
            }
            _ => true,
        }
    }
}

/// Pure deterministic bounded fold.
///
/// `document` is the workflow document the execution runs, when the caller
/// has it. With it, an observation at a site the document lacks is a typed
/// mismatch and never enters the overlay; without it the overlay says so in
/// its coverage and lists what was observed. `records` are the execution's
/// observations: its language facts and the body starts of its admitted
/// steps. The committed settlement is applied with
/// [`WorkflowExecutionOverlay::settle`] and survives every later fold.
///
/// The overlay retains canonical observations, so folding partitions is byte
/// identical to folding their concatenation. Input order is irrelevant;
/// terminal transitions dominate starts for one occurrence; a later
/// occurrence remains visible; identical duplicates disappear; divergent
/// duplicates become typed conflicts. When the limit is exceeded, only
/// identities after the canonical watermark remain eligible, making later
/// batches obey the same truncation decision as a batch fold.
pub fn fold_workflow_overlay(
    previous: Option<&WorkflowExecutionOverlay>,
    document: Option<&WorkflowOverlayDocument>,
    records: &[TraceRecord],
    history_limit: usize,
) -> Result<WorkflowExecutionOverlay, WorkflowOverlayFoldError> {
    if history_limit == 0 {
        return Err(WorkflowOverlayFoldError::ZeroHistoryLimit);
    }
    let incoming = records.iter().filter_map(Incoming::of).collect::<Vec<_>>();
    let previous_execution = previous.map(ExecutionRef::of);
    let (execution_key, subject) = batch_execution(previous_execution.as_ref(), &incoming)
        .ok_or(WorkflowOverlayFoldError::NoExecutionObservations)?;
    for item in &incoming {
        if !item.belongs_to(&execution_key, &subject) {
            let other = item.execution.key();
            return Err(match previous {
                Some(previous) => WorkflowOverlayFoldError::PreviousExecutionMismatch {
                    previous: previous.execution_key.clone(),
                    event: other,
                },
                None => WorkflowOverlayFoldError::MixedExecutions {
                    first: execution_key,
                    other,
                },
            });
        }
    }

    let execution = previous_execution
        .into_iter()
        .chain(incoming.iter().map(|item| item.execution.clone()))
        .reduce(ExecutionRef::canonical)
        .ok_or(WorkflowOverlayFoldError::NoExecutionObservations)?;
    let status = incoming.iter().fold(
        previous.map_or(LanguageExecutionStatus::Running, |overlay| overlay.status),
        |status, item| observed_execution_status(status, &execution.subject, &item.fact),
    );
    let mut history = previous
        .map(|overlay| {
            overlay
                .history
                .iter()
                .cloned()
                .map(|item| (item.identity.clone(), item))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut conflict_variants = previous
        .map(|overlay| {
            overlay
                .conflicts
                .iter()
                .map(|conflict| {
                    (
                        conflict.identity.clone(),
                        conflict.variants.iter().cloned().collect::<BTreeSet<_>>(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut site_retention = previous
        .map(|overlay| {
            overlay
                .retention
                .iter()
                .cloned()
                .map(|retention| (retention.site.clone(), retention))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut document_state = previous.map(DocumentState::of).unwrap_or_default();
    if let Some(document) = document {
        // What an earlier fold retained without the document is held to it
        // now, so supplying the document late equals supplying it first.
        document_state.loaded = Some(document.reference().clone());
        history.retain(|identity, item| document_state.admit(Some(document), identity, &item.fact));
        conflict_variants.retain(|identity, _| {
            identity
                .site
                .as_ref()
                .is_none_or(|site| document.contains(site))
        });
        site_retention.retain(|site, _| {
            let known = document.contains(site);
            if !known {
                document_state
                    .report(WorkflowOverlayMismatch::SiteOutsideDocument { site: site.clone() });
            }
            known
        });
    }

    for item in incoming {
        let event_identity = event_identity(&item.fact);
        if !document_state.admit(document, &event_identity, &item.fact) {
            continue;
        }
        if let Some((site, occurrence)) = site_occurrence(&event_identity)
            && let Some(retention) = site_retention.get_mut(site)
            && occurrence <= retention.truncation_watermark
        {
            merge_late_retained_event(retention, item.timestamp, &item.fact, &execution);
            continue;
        }
        let candidate = WorkflowOverlayHistoryEvent {
            identity: event_identity.clone(),
            timestamp: item.timestamp,
            fact: item.fact,
        };
        match history.entry(event_identity.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(candidate);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if entry.get() != &candidate {
                    let variants = conflict_variants.entry(event_identity).or_default();
                    insert_bounded_variant(variants, history_digest(entry.get()));
                    insert_bounded_variant(variants, history_digest(&candidate));
                    let current = entry.get().clone();
                    entry.insert(canonical_history_event(current, candidate));
                }
            }
        }
    }

    let sites = history
        .keys()
        .filter_map(|identity| identity.site.clone())
        .collect::<BTreeSet<_>>();
    for site in sites {
        loop {
            let occurrences = history
                .keys()
                .filter_map(|identity| {
                    (identity.site.as_ref() == Some(&site))
                        .then_some(identity.occurrence)
                        .flatten()
                })
                .collect::<BTreeSet<_>>();
            if occurrences.len() <= history_limit {
                break;
            }
            let Some(&occurrence) = occurrences.first() else {
                break;
            };
            let dropped_keys = history
                .keys()
                .filter(|identity| {
                    identity.site.as_ref() == Some(&site) && identity.occurrence == Some(occurrence)
                })
                .cloned()
                .collect::<Vec<_>>();
            let dropped = dropped_keys
                .iter()
                .filter_map(|identity| history.remove(identity))
                .collect::<Vec<_>>();
            let prior = site_retention.remove(&site);
            site_retention.insert(
                site.clone(),
                merge_site_retention(prior, &site, occurrence, &dropped, &execution),
            );
        }
    }
    conflict_variants.retain(|identity, _| {
        site_occurrence(identity).is_none_or(|(site, occurrence)| {
            site_retention
                .get(site)
                .is_none_or(|retention| occurrence > retention.truncation_watermark)
        })
    });
    let conflicts = conflict_variants
        .into_iter()
        .map(|(identity, variants)| WorkflowOverlayConflict {
            identity,
            kind: WorkflowOverlayConflictKind::ConflictingDuplicate,
            variants: variants.into_iter().collect(),
        })
        .collect::<Vec<_>>();
    let history = history.into_values().collect::<Vec<_>>();
    Ok(materialize_overlay(
        execution,
        document_state,
        history,
        conflicts,
        history_limit,
        site_retention.into_values().collect(),
        ExecutionProjection {
            status,
            settlement: previous.and_then(|overlay| overlay.settlement),
        },
    ))
}

#[derive(Clone, Copy)]
struct ExecutionProjection {
    status: LanguageExecutionStatus,
    settlement: Option<WorkflowOverlaySettlement>,
}

impl ExecutionProjection {
    fn provisional(status: LanguageExecutionStatus) -> Self {
        Self {
            status,
            settlement: None,
        }
    }
}

fn materialize_overlay(
    execution: ExecutionRef,
    document: DocumentState,
    history: Vec<WorkflowOverlayHistoryEvent>,
    conflicts: Vec<WorkflowOverlayConflict>,
    history_limit: usize,
    retention: Vec<WorkflowOverlaySiteRetention>,
    projection: ExecutionProjection,
) -> WorkflowExecutionOverlay {
    let execution_key = execution.key();
    let mut sites = BTreeMap::new();
    let mut occurrences = BTreeMap::<OccurrenceKey, OccurrenceFold>::new();
    let mut children = BTreeMap::new();
    let mut started_document = None;
    let mut start_observed = false;
    for retained in &retention {
        sites.insert(retained.site.clone(), retained.state.clone());
        for child in &retained.children {
            children.insert(child_link_key(child), child.clone());
        }
    }
    for item in &history {
        let Some(site) = item.identity.site.clone() else {
            if let WorkflowOverlayFact::Language {
                document,
                payload: TraceLanguageExecutionPayload::ExecutionStarted,
                ..
            } = &item.fact
            {
                start_observed = true;
                started_document = Some(document.as_ref().clone());
            }
            continue;
        };
        let state = sites
            .entry(site.clone())
            .or_insert_with(|| WorkflowOverlaySite::unobserved(site.clone()));
        let occurrence = item.identity.occurrence.unwrap_or_default();
        // A child link is a fact about the site, not a transition of one of
        // its occurrences: only the arms that fold one create it.
        macro_rules! folded {
            () => {
                occurrences.entry((site.clone(), occurrence)).or_default()
            };
        }
        match &item.fact {
            WorkflowOverlayFact::StepBodyStarted { step } => {
                let folded = folded!();
                folded.started(item.timestamp);
                folded.bind(step.call_id.clone(), Some(step.attempt));
            }
            WorkflowOverlayFact::Language { payload, .. } => match payload {
                TraceLanguageExecutionPayload::ExecutionStarted
                | TraceLanguageExecutionPayload::ExecutionFinished { .. } => {}
                TraceLanguageExecutionPayload::NodeStarted { call_id, .. } => {
                    let folded = folded!();
                    folded.started(item.timestamp);
                    if let Some(call_id) = call_id {
                        folded.bind(call_id.clone(), None);
                    }
                }
                TraceLanguageExecutionPayload::NodeWaiting { awaited, .. } => {
                    folded!().waiting = Some((item.timestamp, awaited.clone()));
                }
                TraceLanguageExecutionPayload::NodeResumed { .. } => {
                    folded!().resumed = Some(item.timestamp);
                }
                TraceLanguageExecutionPayload::NodeCompleted { call_id, .. } => {
                    let folded = folded!();
                    folded.explicit_terminal = Some(OccurrenceTerminal::Completed(item.timestamp));
                    if let Some(call_id) = call_id {
                        folded.bind(call_id.clone(), None);
                    }
                }
                TraceLanguageExecutionPayload::NodeFailed {
                    call_id, failure, ..
                } => {
                    let folded = folded!();
                    folded.explicit_terminal =
                        Some(OccurrenceTerminal::Failed(item.timestamp, failure.clone()));
                    if let Some(call_id) = call_id {
                        folded.bind(call_id.clone(), None);
                    }
                }
                TraceLanguageExecutionPayload::NodeCancelled { .. } => {
                    folded!().explicit_terminal =
                        Some(OccurrenceTerminal::Cancelled(item.timestamp));
                }
                TraceLanguageExecutionPayload::BranchSelected { selected, .. } => {
                    state.branch = Some(*selected);
                    folded!().provisional_terminal =
                        Some(OccurrenceTerminal::Completed(item.timestamp));
                }
                TraceLanguageExecutionPayload::ChildStarted { child, .. } => {
                    let link = child_link(&execution_key, &site, child);
                    children.insert(child_link_key(&link), link);
                }
            },
        }
    }
    if let Some(settlement) = projection.settlement {
        settle_retained_sites(sites.values_mut(), settlement);
        if let (WorkflowOverlayTerminal::Cancelled, Some(end)) =
            (settlement.terminal, settlement.occurred_at)
        {
            for folded in occurrences.values_mut() {
                if folded_terminal(folded).is_none()
                    && (folded.start.is_some() || folded.waiting.is_some())
                {
                    folded.explicit_terminal = Some(OccurrenceTerminal::Cancelled(end));
                }
            }
        }
    }
    apply_occurrences(&mut sites, &occurrences);
    if let Some(settlement) = projection.settlement {
        settle_incomplete_sites(sites.values_mut(), settlement);
    }
    WorkflowExecutionOverlay {
        schema_version: TRACE_SCHEMA_VERSION,
        execution_key,
        scope: execution.scope,
        subject: execution.subject,
        generation: execution.generation,
        coverage: WorkflowOverlayCoverage {
            document_loaded: document.loaded.is_some(),
            start_observed,
        },
        document: document.loaded.or(started_document),
        status: projection
            .settlement
            .map_or(projection.status, |settlement| {
                settlement.terminal.execution_status()
            }),
        settlement: projection.settlement,
        sites: sites.into_values().collect(),
        children: children.into_values().collect(),
        mismatches: document.mismatches.into_iter().collect(),
        mismatches_truncated: document.truncated,
        history_limit,
        retention,
        conflicts,
        history,
    }
}

#[derive(Default)]
struct OccurrenceFold {
    start: Option<DateTime<Utc>>,
    waiting: Option<(DateTime<Utc>, crate::TraceNodeAwaited)>,
    resumed: Option<DateTime<Utc>>,
    explicit_terminal: Option<OccurrenceTerminal>,
    provisional_terminal: Option<OccurrenceTerminal>,
    call: Option<(lash_sansio::ToolCallId, Option<u32>)>,
}

impl OccurrenceFold {
    /// The occurrence began no later than `timestamp`: a retried body starts
    /// again, and the occurrence still began at its first start.
    fn started(&mut self, timestamp: DateTime<Utc>) {
        self.start = Some(self.start.map_or(timestamp, |start| start.min(timestamp)));
    }

    /// Bind the occurrence to `call_id`. A binding the step's actor stated
    /// (it carries the attempt) outranks one a language fact repeated.
    fn bind(&mut self, call_id: lash_sansio::ToolCallId, attempt: Option<u32>) {
        let stated = matches!(self.call, Some((_, Some(_))));
        if attempt.is_some() || !stated {
            self.call = Some((call_id, attempt));
        }
    }
}

/// The site and which occurrence of it: occurrences count per site.
type OccurrenceKey = (WorkflowSiteRef, u64);

enum OccurrenceTerminal {
    Completed(DateTime<Utc>),
    Failed(DateTime<Utc>, TraceLanguageExecutionFailure),
    Cancelled(DateTime<Utc>),
}

fn apply_occurrences(
    sites: &mut BTreeMap<WorkflowSiteRef, WorkflowOverlaySite>,
    occurrences: &BTreeMap<OccurrenceKey, OccurrenceFold>,
) {
    for (site, state) in sites {
        let matching = occurrences
            .iter()
            .filter(|((of, _), _)| of == site)
            .collect::<Vec<_>>();
        state.summary.retained_occurrences += matching.len() as u64;
        state.summary.started_count += matching
            .iter()
            .filter(|(_, occurrence)| occurrence.start.is_some())
            .count() as u64;
        let terminals = matching
            .iter()
            .filter_map(|((_, occurrence), folded)| {
                folded_terminal(folded).map(|terminal| {
                    let (status, end) = match terminal {
                        OccurrenceTerminal::Completed(end) => {
                            (WorkflowOverlayTerminalStatus::Completed, end.to_owned())
                        }
                        OccurrenceTerminal::Failed(end, _) => {
                            (WorkflowOverlayTerminalStatus::Failed, end.to_owned())
                        }
                        OccurrenceTerminal::Cancelled(end) => {
                            (WorkflowOverlayTerminalStatus::Cancelled, end.to_owned())
                        }
                    };
                    WorkflowOverlayTerminalRecord {
                        occurrence: *occurrence,
                        status,
                        end,
                    }
                })
            })
            .collect::<Vec<_>>();
        state.summary.terminal_count += terminals.len() as u64;
        state.summary.first_terminal = [
            state.summary.first_terminal.clone(),
            terminals.first().cloned(),
        ]
        .into_iter()
        .flatten()
        .min_by_key(|terminal| terminal.occurrence);
        state.summary.last_terminal = [
            state.summary.last_terminal.clone(),
            terminals.last().cloned(),
        ]
        .into_iter()
        .flatten()
        .max_by_key(|terminal| terminal.occurrence);
        if let Some(((_, occurrence), folded)) = matching
            .iter()
            .rev()
            .find(|(_, folded)| folded.call.is_some())
            && let Some((call_id, attempt)) = &folded.call
        {
            state.call = Some(WorkflowOverlayCall {
                occurrence: *occurrence,
                call_id: call_id.clone(),
                attempt: *attempt,
            });
        }
        if let Some(((_, occurrence), folded)) = matching.last() {
            state.occurrence = match folded_terminal(folded) {
                Some(OccurrenceTerminal::Completed(end)) => WorkflowOverlayOccurrence::Completed {
                    occurrence: *occurrence,
                    start: folded.start,
                    end: end.to_owned(),
                    duration_ms: folded
                        .start
                        .map(|start| end.signed_duration_since(start).num_milliseconds().max(0)),
                },
                Some(OccurrenceTerminal::Failed(end, failure)) => {
                    WorkflowOverlayOccurrence::Failed {
                        occurrence: *occurrence,
                        start: folded.start,
                        end: end.to_owned(),
                        duration_ms: folded.start.map(|start| {
                            end.signed_duration_since(start).num_milliseconds().max(0)
                        }),
                        failure: failure.clone(),
                    }
                }
                Some(OccurrenceTerminal::Cancelled(end)) => WorkflowOverlayOccurrence::Cancelled {
                    occurrence: *occurrence,
                    start: folded.start,
                    end: *end,
                },
                None => match &folded.waiting {
                    Some((since, awaited))
                        if folded.resumed.is_none_or(|resumed| resumed < *since) =>
                    {
                        WorkflowOverlayOccurrence::Waiting {
                            occurrence: *occurrence,
                            start: folded.start,
                            since: *since,
                            awaited: awaited.clone(),
                        }
                    }
                    _ => folded
                        .start
                        .map(|start| WorkflowOverlayOccurrence::Running {
                            occurrence: *occurrence,
                            start,
                        })
                        .unwrap_or_default(),
                },
            };
        }
    }
}

fn folded_terminal(folded: &OccurrenceFold) -> Option<&OccurrenceTerminal> {
    let terminal = folded
        .explicit_terminal
        .as_ref()
        .or(folded.provisional_terminal.as_ref());
    match terminal {
        Some(OccurrenceTerminal::Cancelled(_))
            if folded.start.is_none() && folded.waiting.is_none() =>
        {
            None
        }
        _ => terminal,
    }
}

fn site_occurrence(identity: &WorkflowOverlayEventIdentity) -> Option<(&WorkflowSiteRef, u64)> {
    Some((identity.site.as_ref()?, identity.occurrence?))
}

fn child_link(
    execution_key: &str,
    site: &WorkflowSiteRef,
    child: &crate::TraceLanguageChildExecution,
) -> WorkflowOverlayChildLink {
    WorkflowOverlayChildLink {
        parent_execution_key: execution_key.to_owned(),
        parent_site: site.clone(),
        child_execution_key: child.graph_key(),
        child_process_id: child.process_id.clone(),
        child_attempt: child.attempt,
        document: child.document.clone(),
    }
}

fn child_link_key(
    child: &WorkflowOverlayChildLink,
) -> (WorkflowSiteRef, lash_sansio::ProcessId, Option<u32>) {
    (
        child.parent_site.clone(),
        child.child_process_id.clone(),
        child.child_attempt,
    )
}

#[expect(
    clippy::expect_used,
    reason = "the dropped history passed to this function contains an event for the site"
)]
fn merge_site_retention(
    prior: Option<WorkflowOverlaySiteRetention>,
    site: &WorkflowSiteRef,
    occurrence: u64,
    dropped: &[WorkflowOverlayHistoryEvent],
    execution: &ExecutionRef,
) -> WorkflowOverlaySiteRetention {
    let archived = prior
        .as_ref()
        .map(|retention| Box::new(retention.state.clone()));
    let prior = prior.into_iter().collect::<Vec<_>>();
    let overlay = materialize_overlay(
        execution.clone(),
        DocumentState::default(),
        dropped.to_vec(),
        Vec::new(),
        1,
        prior,
        ExecutionProjection::provisional(LanguageExecutionStatus::Running),
    );
    let state = overlay
        .sites
        .into_iter()
        .find(|state| state.site == *site)
        .expect("an evicted site event materializes its site");
    WorkflowOverlaySiteRetention {
        site: site.clone(),
        truncation_watermark: occurrence,
        archived,
        watermark_history: dropped.to_vec(),
        state,
        children: overlay
            .children
            .into_iter()
            .filter(|child| child.parent_site == *site)
            .collect(),
    }
}

#[expect(
    clippy::expect_used,
    reason = "the watermark history contains the retained site whose matching event was merged"
)]
fn merge_late_retained_event(
    retention: &mut WorkflowOverlaySiteRetention,
    timestamp: DateTime<Utc>,
    fact: &WorkflowOverlayFact,
    execution: &ExecutionRef,
) {
    let identity = event_identity(fact);
    if identity.occurrence == Some(retention.truncation_watermark) {
        let candidate = WorkflowOverlayHistoryEvent {
            identity: identity.clone(),
            timestamp,
            fact: fact.clone(),
        };
        if let Some(index) = retention
            .watermark_history
            .iter()
            .position(|current| current.identity == identity)
        {
            let current = retention.watermark_history[index].clone();
            retention.watermark_history[index] = canonical_history_event(current, candidate);
        } else {
            retention.watermark_history.push(candidate);
            retention
                .watermark_history
                .sort_by(|left, right| left.identity.cmp(&right.identity));
        }
        let watermark = materialize_overlay(
            execution.clone(),
            DocumentState::default(),
            retention.watermark_history.clone(),
            Vec::new(),
            1,
            Vec::new(),
            ExecutionProjection::provisional(LanguageExecutionStatus::Running),
        );
        let mut watermark_state = watermark
            .sites
            .into_iter()
            .find(|state| state.site == retention.site)
            .expect("watermark history materializes its site");
        if let Some(archived) = &retention.archived {
            merge_site_report(&mut watermark_state.summary, &archived.summary);
        }
        retention.state = watermark_state;
        for child in watermark.children {
            if !retention
                .children
                .iter()
                .any(|current| child_link_key(current) == child_link_key(&child))
            {
                retention.children.push(child);
            }
        }
        retention.children.sort_by_key(child_link_key);
        return;
    }
    let occurrence = identity.occurrence;
    match fact {
        WorkflowOverlayFact::StepBodyStarted { .. }
        | WorkflowOverlayFact::Language {
            payload: TraceLanguageExecutionPayload::NodeStarted { .. },
            ..
        } => {
            let start = match &retention.state.occurrence {
                WorkflowOverlayOccurrence::Running {
                    occurrence: retained,
                    start,
                } if Some(*retained) == occurrence => Some((*start).min(timestamp)),
                WorkflowOverlayOccurrence::Completed {
                    occurrence: retained,
                    start,
                    ..
                }
                | WorkflowOverlayOccurrence::Failed {
                    occurrence: retained,
                    start,
                    ..
                }
                | WorkflowOverlayOccurrence::Cancelled {
                    occurrence: retained,
                    start,
                    ..
                }
                | WorkflowOverlayOccurrence::Waiting {
                    occurrence: retained,
                    start,
                    ..
                } if Some(*retained) == occurrence => {
                    Some(start.map_or(timestamp, |start| start.min(timestamp)))
                }
                _ => None,
            };
            if let Some(start) = start {
                let previously_missing = match &retention.state.occurrence {
                    WorkflowOverlayOccurrence::Completed { start, .. }
                    | WorkflowOverlayOccurrence::Failed { start, .. }
                    | WorkflowOverlayOccurrence::Cancelled { start, .. }
                    | WorkflowOverlayOccurrence::Waiting { start, .. } => start.is_none(),
                    _ => false,
                };
                retention.state.occurrence = match &retention.state.occurrence {
                    WorkflowOverlayOccurrence::Running { occurrence, .. } => {
                        WorkflowOverlayOccurrence::Running {
                            occurrence: *occurrence,
                            start,
                        }
                    }
                    WorkflowOverlayOccurrence::Waiting {
                        occurrence,
                        since,
                        awaited,
                        ..
                    } => WorkflowOverlayOccurrence::Waiting {
                        occurrence: *occurrence,
                        start: Some(start),
                        since: *since,
                        awaited: awaited.clone(),
                    },
                    WorkflowOverlayOccurrence::Completed {
                        occurrence, end, ..
                    } => WorkflowOverlayOccurrence::Completed {
                        occurrence: *occurrence,
                        start: Some(start),
                        end: *end,
                        duration_ms: Some(
                            end.signed_duration_since(start).num_milliseconds().max(0),
                        ),
                    },
                    WorkflowOverlayOccurrence::Failed {
                        occurrence,
                        end,
                        failure,
                        ..
                    } => WorkflowOverlayOccurrence::Failed {
                        occurrence: *occurrence,
                        start: Some(start),
                        end: *end,
                        duration_ms: Some(
                            end.signed_duration_since(start).num_milliseconds().max(0),
                        ),
                        failure: failure.clone(),
                    },
                    WorkflowOverlayOccurrence::Cancelled {
                        occurrence, end, ..
                    } => WorkflowOverlayOccurrence::Cancelled {
                        occurrence: *occurrence,
                        start: Some(start),
                        end: *end,
                    },
                    WorkflowOverlayOccurrence::Unobserved
                    | WorkflowOverlayOccurrence::Incomplete { .. } => return,
                };
                if previously_missing {
                    retention.state.summary.started_count += 1;
                }
            }
        }
        WorkflowOverlayFact::Language { payload, .. } => match payload {
            TraceLanguageExecutionPayload::NodeCompleted { occurrence, .. }
            | TraceLanguageExecutionPayload::NodeFailed { occurrence, .. }
            | TraceLanguageExecutionPayload::NodeCancelled { occurrence, .. } => {
                if retention.state.occurrence.is_terminal() {
                    return;
                }
                if matches!(payload, TraceLanguageExecutionPayload::NodeCancelled { .. })
                    && !matches!(&retention.state.occurrence,
                        WorkflowOverlayOccurrence::Running { occurrence: retained, .. }
                        | WorkflowOverlayOccurrence::Waiting { occurrence: retained, .. }
                        if retained == occurrence)
                {
                    return;
                }
                let start = match retention.state.occurrence {
                    WorkflowOverlayOccurrence::Running {
                        occurrence: retained,
                        start,
                    } if retained == *occurrence => Some(start),
                    _ => None,
                };
                let duration_ms = start.map(|start| {
                    timestamp
                        .signed_duration_since(start)
                        .num_milliseconds()
                        .max(0)
                });
                let (status, observation) = match payload {
                    TraceLanguageExecutionPayload::NodeFailed { failure, .. } => (
                        WorkflowOverlayTerminalStatus::Failed,
                        WorkflowOverlayOccurrence::Failed {
                            occurrence: *occurrence,
                            start,
                            end: timestamp,
                            duration_ms,
                            failure: failure.clone(),
                        },
                    ),
                    TraceLanguageExecutionPayload::NodeCancelled { .. } => (
                        WorkflowOverlayTerminalStatus::Cancelled,
                        WorkflowOverlayOccurrence::Cancelled {
                            occurrence: *occurrence,
                            start,
                            end: timestamp,
                        },
                    ),
                    _ => (
                        WorkflowOverlayTerminalStatus::Completed,
                        WorkflowOverlayOccurrence::Completed {
                            occurrence: *occurrence,
                            start,
                            end: timestamp,
                            duration_ms,
                        },
                    ),
                };
                retention.state.occurrence = observation;
                retention.state.summary.terminal_count += 1;
                let terminal = WorkflowOverlayTerminalRecord {
                    occurrence: *occurrence,
                    status,
                    end: timestamp,
                };
                retention.state.summary.first_terminal = Some(terminal.clone());
                retention.state.summary.last_terminal = Some(terminal);
            }
            TraceLanguageExecutionPayload::BranchSelected { selected, .. } => {
                retention.state.branch = Some(*selected);
            }
            TraceLanguageExecutionPayload::ChildStarted { child, .. } => {
                let link = child_link(&execution.key(), &retention.site, child);
                if !retention
                    .children
                    .iter()
                    .any(|current| child_link_key(current) == child_link_key(&link))
                {
                    retention.children.push(link);
                    retention.children.sort_by_key(child_link_key);
                }
            }
            TraceLanguageExecutionPayload::ExecutionStarted
            | TraceLanguageExecutionPayload::ExecutionFinished { .. }
            | TraceLanguageExecutionPayload::NodeStarted { .. }
            | TraceLanguageExecutionPayload::NodeWaiting { .. }
            | TraceLanguageExecutionPayload::NodeResumed { .. } => {}
        },
    }
}

fn merge_site_report(target: &mut WorkflowOverlaySiteReport, archived: &WorkflowOverlaySiteReport) {
    target.retained_occurrences += archived.retained_occurrences;
    target.started_count += archived.started_count;
    target.terminal_count += archived.terminal_count;
    target.first_terminal = [
        target.first_terminal.clone(),
        archived.first_terminal.clone(),
    ]
    .into_iter()
    .flatten()
    .min_by_key(|terminal| terminal.occurrence);
    target.last_terminal = [target.last_terminal.clone(), archived.last_terminal.clone()]
        .into_iter()
        .flatten()
        .max_by_key(|terminal| terminal.occurrence);
}

fn event_identity(fact: &WorkflowOverlayFact) -> WorkflowOverlayEventIdentity {
    use WorkflowOverlayEventTransition as Transition;
    let payload = match fact {
        WorkflowOverlayFact::StepBodyStarted { step } => {
            return WorkflowOverlayEventIdentity {
                site: Some(step.site()),
                occurrence: Some(step.occurrence),
                transition: Transition::StepBodyStarted,
                step_attempt: Some(step.attempt),
            };
        }
        WorkflowOverlayFact::Language { payload, .. } => payload,
    };
    let transition = match payload {
        TraceLanguageExecutionPayload::ExecutionStarted => Transition::ExecutionStarted,
        TraceLanguageExecutionPayload::ExecutionFinished { .. } => Transition::ExecutionFinished,
        TraceLanguageExecutionPayload::NodeStarted { .. } => Transition::NodeStarted,
        TraceLanguageExecutionPayload::NodeWaiting { .. } => Transition::NodeWaiting,
        TraceLanguageExecutionPayload::NodeResumed { .. } => Transition::NodeResumed,
        TraceLanguageExecutionPayload::NodeCompleted { .. }
        | TraceLanguageExecutionPayload::NodeFailed { .. }
        | TraceLanguageExecutionPayload::NodeCancelled { .. } => Transition::NodeTerminal,
        TraceLanguageExecutionPayload::BranchSelected { .. } => Transition::BranchSelected,
        TraceLanguageExecutionPayload::ChildStarted { .. } => Transition::ChildStarted,
    };
    let (site, occurrence) = payload.occurrence_key().unzip();
    WorkflowOverlayEventIdentity {
        site,
        occurrence,
        transition,
        step_attempt: None,
    }
}

fn canonical_value<T: Serialize>(left: T, right: T) -> T {
    if canonical_bytes(&left) <= canonical_bytes(&right) {
        left
    } else {
        right
    }
}

fn canonical_history_event(
    left: WorkflowOverlayHistoryEvent,
    right: WorkflowOverlayHistoryEvent,
) -> WorkflowOverlayHistoryEvent {
    match canonical_execution_status_of(&left.fact, &right.fact) {
        Some(true) => left,
        Some(false) => right,
        None => canonical_value(left, right),
    }
}

fn canonical_execution_status(
    left: LanguageExecutionStatus,
    right: LanguageExecutionStatus,
) -> LanguageExecutionStatus {
    match (left.is_terminal(), right.is_terminal()) {
        (true, false) => left,
        (false, true) => right,
        (true, true) => canonical_value(left, right),
        (false, false) => LanguageExecutionStatus::Running,
    }
}

#[expect(
    clippy::expect_used,
    reason = "the fold only serializes its own infallible in-memory trace value types"
)]
fn canonical_bytes(value: &impl Serialize) -> Vec<u8> {
    serde_json::to_vec(value).expect("workflow overlay fold values serialize")
}

fn history_digest(value: &WorkflowOverlayHistoryEvent) -> String {
    format!("sha256:{:x}", Sha256::digest(canonical_bytes(value)))
}

fn insert_bounded_variant(variants: &mut BTreeSet<String>, variant: String) {
    variants.insert(variant);
    while variants.len() > 2 {
        let middle = variants.iter().nth(1).cloned();
        if let Some(middle) = middle {
            variants.remove(&middle);
        }
    }
}

#[cfg(test)]
mod tests;
