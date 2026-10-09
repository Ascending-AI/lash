//! The execution overlay of a workflow document: a bounded, deterministic
//! fold of what one execution was observed to do, keyed by the document's
//! execution sites.
//!
//! The reducer reads three things: the immutable document (as the site index
//! [`WorkflowOverlayDocument`]), the provisional observations of the
//! execution, and the committed settlement of its process. It copies nothing
//! static out of the document.
//!
//! There is one reducer, [`WorkflowExecutionOverlayAccumulator`]. The pure
//! [`fold_workflow_overlay`] resumes one from a previous overlay, appends and
//! snapshots.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    TRACE_SCHEMA_VERSION, TraceBranchSelection, TraceEvent, TraceLanguageChildExecution,
    TraceLanguageExecutionFailure, TraceLanguageExecutionGeneration,
    TraceLanguageExecutionIdentity as LanguageIdentity, TraceLanguageExecutionPayload,
    TraceLanguageExecutionStatus as LanguageExecutionStatus, TraceNodeFact, TraceRecord,
    TraceRuntimeScope, TraceRuntimeSubject, WorkflowDocumentRef,
};

mod accumulator;
mod model;
mod occurrence;
mod retention;
mod settlement;
pub use accumulator::WorkflowExecutionOverlayAccumulator;
pub use model::*;
use occurrence::{
    ExecutionHistory, Observation, OccurrenceHistory, Placement, SiteChildren, Transition,
};
use retention::SiteRetention;
use settlement::{is_process_subject, settle_incomplete_site, settle_retained_site};

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

/// The execution an overlay is of: who ran, and under which name.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ExecutionRef {
    subject: TraceRuntimeSubject,
    /// The scope and attempt the execution's own observations state. A step
    /// names only its process, so an execution known from steps alone is
    /// not yet named.
    name: Option<ExecutionName>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExecutionName {
    scope: TraceRuntimeScope,
    generation: Option<TraceLanguageExecutionGeneration>,
}

impl ExecutionRef {
    fn of_language(identity: &LanguageIdentity) -> Self {
        Self {
            subject: identity.subject.clone(),
            name: Some(ExecutionName {
                scope: identity.scope.clone(),
                generation: identity.generation,
            }),
        }
    }

    /// The execution a process step belongs to: the process's own.
    fn of_step(step: &crate::StepBodyStarted) -> Self {
        Self {
            subject: TraceRuntimeSubject::Process {
                process_id: step.process_id.clone(),
            },
            name: None,
        }
    }

    fn generation(&self) -> Option<TraceLanguageExecutionGeneration> {
        self.name.as_ref().and_then(|name| name.generation)
    }

    fn key(&self) -> String {
        execution_key(&self.subject, self.generation())
    }

    /// Whether `other` is an observation of this execution: the same
    /// subject and, once both are named, the same attempt.
    fn admits(&self, other: &Self) -> bool {
        self.subject == other.subject
            && match (&self.name, &other.name) {
                (Some(held), Some(offered)) => held.generation == offered.generation,
                _ => true,
            }
    }

    /// Merge an admitted observation's reference by rule: an unnamed
    /// execution adopts the name, and a step, which carries none, changes
    /// nothing. Of two names, a scope field one knows fills the other's
    /// gap, and a field both know keeps the value held.
    fn adopt(&mut self, other: &Self) {
        let Some(offered) = &other.name else {
            return;
        };
        match &mut self.name {
            None => self.name = Some(offered.clone()),
            Some(held) => {
                let (held, offered) = (&mut held.scope, &offered.scope);
                if held.session_id.is_none() {
                    held.session_id.clone_from(&offered.session_id);
                }
                if held.turn_id.is_none() {
                    held.turn_id.clone_from(&offered.turn_id);
                }
                held.turn_index = held.turn_index.or(offered.turn_index);
                held.protocol_iteration = held.protocol_iteration.or(offered.protocol_iteration);
            }
        }
    }
}

/// One observation as the reducer takes it in.
struct Incoming {
    execution: ExecutionRef,
    observation: Observation,
}

impl Incoming {
    fn of(record: &TraceRecord) -> Option<Self> {
        let (execution, fact) = match &record.event {
            TraceEvent::LanguageExecution { event, .. } => (
                ExecutionRef::of_language(&event.identity),
                WorkflowOverlayFact::Language {
                    document: Box::new(event.identity.document.clone()),
                    payload: event.payload.clone(),
                },
            ),
            TraceEvent::StepBodyStarted { step } => (
                ExecutionRef::of_step(step),
                WorkflowOverlayFact::StepBodyStarted { step: step.clone() },
            ),
            _ => return None,
        };
        Some(Self {
            execution,
            observation: Observation {
                timestamp: record.timestamp,
                fact,
            },
        })
    }
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
            loaded: match &overlay.document {
                WorkflowOverlayDocumentBinding::Loaded { reference } => Some(reference.clone()),
                WorkflowOverlayDocumentBinding::Unknown
                | WorkflowOverlayDocumentBinding::Claimed { .. } => None,
            },
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

    /// Hold the start's claim to `document`. A start that names another
    /// document is reported and kept: the execution did start.
    fn claim(&mut self, document: &WorkflowOverlayDocument, claimed: &WorkflowDocumentRef) {
        if claimed != document.reference() {
            self.report(WorkflowOverlayMismatch::Document {
                claimed: claimed.clone(),
            });
        }
    }

    /// Whether `site` belongs to `document`. One that does not is reported.
    fn admits(&mut self, document: &WorkflowOverlayDocument, site: &WorkflowTaskSite) -> bool {
        let known = document.contains(&site.site);
        if !known {
            self.report(WorkflowOverlayMismatch::SiteOutsideDocument { site: site.clone() });
        }
        known
    }
}

/// Pure deterministic bounded fold.
///
/// `document` is the workflow document the execution runs, when the caller
/// has it. With it, an observation at a site the document lacks is a typed
/// mismatch and never enters the overlay; without it the overlay says so in
/// its document binding and lists what was observed. `records` are the
/// execution's observations: its language facts and the body starts of its
/// admitted steps. The committed settlement is applied with
/// [`WorkflowExecutionOverlay::settle`] and survives every later fold.
///
/// The overlay retains canonical observations, so folding partitions equals
/// folding their concatenation. Input order is irrelevant; terminal
/// transitions dominate starts for one occurrence; a later occurrence
/// remains visible; an occurrence may wait and resume any number of times;
/// identical duplicates disappear; two different observations of one
/// transition become a typed conflict, and the earlier one is kept. Each
/// site retains at most `history_limit` occurrences: what it evicts is
/// summarized by its [`WorkflowOverlaySiteRetention`].
pub fn fold_workflow_overlay(
    previous: Option<&WorkflowExecutionOverlay>,
    document: Option<&WorkflowOverlayDocument>,
    records: &[TraceRecord],
    history_limit: usize,
) -> Result<WorkflowExecutionOverlay, WorkflowOverlayFoldError> {
    if history_limit == 0 {
        return Err(WorkflowOverlayFoldError::ZeroHistoryLimit);
    }
    let mut accumulator = match previous {
        Some(previous) => WorkflowExecutionOverlayAccumulator::resume(previous, history_limit),
        None => WorkflowExecutionOverlayAccumulator::new(history_limit),
    };
    if let Some(document) = document {
        accumulator.set_document(document.clone());
    }
    accumulator.fold(records)?;
    accumulator
        .snapshot()
        .ok_or(WorkflowOverlayFoldError::NoExecutionObservations)
}

#[cfg(test)]
mod tests;
