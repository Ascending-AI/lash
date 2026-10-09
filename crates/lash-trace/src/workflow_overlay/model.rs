use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use lash_sansio::WorkflowSiteRef;
use serde::{Deserialize, Serialize};

use crate::{
    StepBodyStarted, TraceBranchSelection, TraceLanguageExecutionFailure,
    TraceLanguageExecutionGeneration, TraceLanguageExecutionPayload,
    TraceLanguageExecutionStatus as LanguageExecutionStatus, TraceRuntimeScope,
    TraceRuntimeSubject, WorkflowDocumentRef, ensure_trace_schema_version,
};

/// Default number of occurrence histories retained for each site.
pub const DEFAULT_WORKFLOW_OVERLAY_HISTORY_LIMIT: usize = 256;

/// Mismatches an overlay lists before it only says there were more.
pub(super) const MISMATCH_LIMIT: usize = 64;

/// What the reducer reads of a workflow document: which document it is and
/// the execution sites it has. It is an index for one fold, built from the
/// document a host loaded; it is not a second copy of the graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowOverlayDocument {
    reference: WorkflowDocumentRef,
    sites: BTreeSet<WorkflowSiteRef>,
}

impl WorkflowOverlayDocument {
    /// The document `reference` names, with the execution `sites` of the
    /// entry it selects.
    pub fn new(
        reference: WorkflowDocumentRef,
        sites: impl IntoIterator<Item = WorkflowSiteRef>,
    ) -> Self {
        Self {
            reference,
            sites: sites.into_iter().collect(),
        }
    }

    pub fn reference(&self) -> &WorkflowDocumentRef {
        &self.reference
    }

    /// Whether the document has `site`.
    pub fn contains(&self, site: &WorkflowSiteRef) -> bool {
        self.sites.contains(site)
    }
}

/// How much of the execution the overlay can speak for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowOverlayCoverage {
    /// The reducer was given the document: every listed site is one of its
    /// sites, and a site it lacks is a mismatch.
    pub document_loaded: bool,
    /// The retained observations include the execution's start. Without it
    /// the observer attached late or lost continuity, and occurrences that
    /// ran before are not here.
    pub start_observed: bool,
}

impl WorkflowOverlayCoverage {
    /// Whether the overlay covers the execution from its start against its
    /// document. Per-site truncation is reported by the retention records.
    pub const fn is_complete(self) -> bool {
        self.document_loaded && self.start_observed
    }
}

/// An observation that does not belong to the claimed document. It is
/// reported and never folded into the overlay.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowOverlayMismatch {
    /// The execution's start names another document than the one loaded.
    Document { claimed: WorkflowDocumentRef },
    /// An observation names a site the loaded document does not have.
    SiteOutsideDocument { site: WorkflowSiteRef },
}

/// Canonical identity of one fold input within its execution.
///
/// Site transitions use `(site, occurrence)` plus the transition kind:
/// occurrences count per site. The transition kind lets a start and its
/// terminal fact merge monotonically while still making a second, different
/// start or terminal a typed conflict. A step body start is one fact per
/// attempt of its body.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct WorkflowOverlayEventIdentity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<WorkflowSiteRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<u64>,
    pub transition: WorkflowOverlayEventTransition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_attempt: Option<u32>,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowOverlayEventTransition {
    ExecutionStarted,
    ExecutionFinished,
    NodeStarted,
    StepBodyStarted,
    NodeWaiting,
    NodeResumed,
    NodeTerminal,
    BranchSelected,
    ChildStarted,
}

/// One observation the overlay folds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "fact", rename_all = "snake_case")]
pub enum WorkflowOverlayFact {
    /// What the language execution reported.
    Language {
        #[serde(flatten)]
        payload: TraceLanguageExecutionPayload,
    },
    /// The admitted body of a process step started.
    StepBodyStarted { step: StepBodyStarted },
}

/// One canonical observation retained so a persisted overlay can be folded
/// again.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowOverlayHistoryEvent {
    pub identity: WorkflowOverlayEventIdentity,
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub fact: WorkflowOverlayFact,
}

#[cfg(test)]
thread_local! {
    pub(super) static HISTORY_CLONES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Clone for WorkflowOverlayHistoryEvent {
    fn clone(&self) -> Self {
        #[cfg(test)]
        HISTORY_CLONES.with(|count| count.set(count.get() + 1));
        Self {
            identity: self.identity.clone(),
            timestamp: self.timestamp,
            fact: self.fact.clone(),
        }
    }
}

/// Why two records under one logical identity did not deduplicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowOverlayConflictKind {
    ConflictingDuplicate,
}

/// A bounded, typed record of divergent values under one logical identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowOverlayConflict {
    pub identity: WorkflowOverlayEventIdentity,
    pub kind: WorkflowOverlayConflictKind,
    pub variants: Vec<String>,
}

/// What one execution did, keyed into the workflow document it ran.
///
/// The overlay holds only what was observed: per-site occurrence states,
/// branch choices, child links and the bounded history they fold from. The
/// static structure (nodes, labels, kinds, edges, the arms of a branch) is
/// the document's; a site with no observation is not listed here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct WorkflowExecutionOverlay {
    pub schema_version: u32,
    /// The execution's key: its subject and, for an attempted execution, its
    /// attempt. Two runs of one document never share it.
    pub execution_key: String,
    pub scope: TraceRuntimeScope,
    pub subject: TraceRuntimeSubject,
    #[serde(flatten)]
    pub generation: Option<TraceLanguageExecutionGeneration>,
    /// The document the execution runs: the loaded document's reference, or
    /// the one the execution's start named when none was loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<WorkflowDocumentRef>,
    pub coverage: WorkflowOverlayCoverage,
    pub status: LanguageExecutionStatus,
    /// Authoritative process settlement, independent of provisional VM outcomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settlement: Option<WorkflowOverlaySettlement>,
    pub sites: Vec<WorkflowOverlaySite>,
    pub children: Vec<WorkflowOverlayChildLink>,
    /// Observations that did not belong to the document, smallest first, at
    /// most 64.
    pub mismatches: Vec<WorkflowOverlayMismatch>,
    /// More mismatches were observed than `mismatches` lists.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mismatches_truncated: bool,
    pub history_limit: usize,
    pub retention: Vec<WorkflowOverlaySiteRetention>,
    pub conflicts: Vec<WorkflowOverlayConflict>,
    pub history: Vec<WorkflowOverlayHistoryEvent>,
}

#[derive(Deserialize)]
struct WorkflowExecutionOverlayWire {
    schema_version: u32,
    execution_key: String,
    scope: TraceRuntimeScope,
    subject: TraceRuntimeSubject,
    attempt: Option<u32>,
    document: Option<WorkflowDocumentRef>,
    coverage: WorkflowOverlayCoverage,
    status: LanguageExecutionStatus,
    settlement: Option<WorkflowOverlaySettlement>,
    sites: Vec<WorkflowOverlaySite>,
    children: Vec<WorkflowOverlayChildLink>,
    mismatches: Vec<WorkflowOverlayMismatch>,
    #[serde(default)]
    mismatches_truncated: bool,
    history_limit: usize,
    retention: Vec<WorkflowOverlaySiteRetention>,
    conflicts: Vec<WorkflowOverlayConflict>,
    history: Vec<WorkflowOverlayHistoryEvent>,
}

impl<'de> Deserialize<'de> for WorkflowExecutionOverlay {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let schema_version = value
            .get("schema_version")
            .cloned()
            .ok_or_else(|| serde::de::Error::missing_field("schema_version"))
            .and_then(|value| serde_json::from_value(value).map_err(serde::de::Error::custom))?;
        ensure_trace_schema_version(schema_version).map_err(serde::de::Error::custom)?;
        let wire: WorkflowExecutionOverlayWire =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self {
            schema_version: wire.schema_version,
            execution_key: wire.execution_key,
            scope: wire.scope,
            subject: wire.subject,
            generation: wire.attempt.map(TraceLanguageExecutionGeneration::new),
            document: wire.document,
            coverage: wire.coverage,
            status: wire.status,
            settlement: wire.settlement,
            sites: wire.sites,
            children: wire.children,
            mismatches: wire.mismatches,
            mismatches_truncated: wire.mismatches_truncated,
            history_limit: wire.history_limit,
            retention: wire.retention,
            conflicts: wire.conflicts,
            history: wire.history,
        })
    }
}

/// Aggregate facts for occurrences evicted from one site's bounded history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowOverlaySiteRetention {
    pub site: WorkflowSiteRef,
    pub truncation_watermark: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<Box<WorkflowOverlaySite>>,
    pub watermark_history: Vec<WorkflowOverlayHistoryEvent>,
    pub state: WorkflowOverlaySite,
    pub children: Vec<WorkflowOverlayChildLink>,
}

/// One occurrence's observed state at a site.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorkflowOverlayOccurrence {
    #[default]
    Unobserved,
    Running {
        occurrence: u64,
        start: DateTime<Utc>,
    },
    Waiting {
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<DateTime<Utc>>,
        since: DateTime<Utc>,
        awaited: crate::TraceNodeAwaited,
    },
    Completed {
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<DateTime<Utc>>,
        end: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<i64>,
    },
    Failed {
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<DateTime<Utc>>,
        end: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<i64>,
        failure: TraceLanguageExecutionFailure,
    },
    Cancelled {
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<DateTime<Utc>>,
        end: DateTime<Utc>,
    },
    /// The process settled without complete terminal evidence for this
    /// observed occurrence. The category is authoritative; success, failure
    /// and timing are not inferred from it.
    Incomplete {
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<DateTime<Utc>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        settled_at: Option<DateTime<Utc>>,
        terminal: WorkflowOverlayTerminal,
    },
}

impl WorkflowOverlayOccurrence {
    /// Whether this occurrence is no longer in flight. An incomplete outcome
    /// can still be refined by retained evidence without reopening execution.
    pub const fn is_terminal(&self) -> bool {
        match self {
            Self::Unobserved | Self::Running { .. } | Self::Waiting { .. } => false,
            Self::Completed { .. }
            | Self::Failed { .. }
            | Self::Cancelled { .. }
            | Self::Incomplete { .. } => true,
        }
    }

    /// Which occurrence of the site this state is of.
    pub const fn occurrence(&self) -> Option<u64> {
        match self {
            Self::Unobserved => None,
            Self::Running { occurrence, .. }
            | Self::Waiting { occurrence, .. }
            | Self::Completed { occurrence, .. }
            | Self::Failed { occurrence, .. }
            | Self::Cancelled { occurrence, .. }
            | Self::Incomplete { occurrence, .. } => Some(*occurrence),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowOverlaySiteReport {
    pub retained_occurrences: u64,
    pub started_count: u64,
    pub terminal_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_terminal: Option<WorkflowOverlayTerminalRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_terminal: Option<WorkflowOverlayTerminalRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowOverlayTerminalRecord {
    pub occurrence: u64,
    pub status: WorkflowOverlayTerminalStatus,
    pub end: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowOverlayTerminalStatus {
    Completed,
    Failed,
    Cancelled,
}

/// The admitted call an occurrence is bound to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowOverlayCall {
    pub occurrence: u64,
    pub call_id: lash_sansio::ToolCallId,
    /// The latest attempt of the call's admitted body that was observed to
    /// start; absent when the binding came from a language fact alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

/// What was observed at one execution site of the document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowOverlaySite {
    pub site: WorkflowSiteRef,
    /// Which arm a branch site last took. Absent on every other site, and on
    /// a branch whose selection has not been observed. The arms themselves
    /// are the document's: a host reads which nodes an unselected arm holds
    /// from the branch's typed children.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<TraceBranchSelection>,
    /// The admitted call of the site's latest bound occurrence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<WorkflowOverlayCall>,
    /// The state of the site's latest retained occurrence.
    #[serde(flatten)]
    pub occurrence: WorkflowOverlayOccurrence,
    pub summary: WorkflowOverlaySiteReport,
}

impl WorkflowOverlaySite {
    pub(super) fn unobserved(site: WorkflowSiteRef) -> Self {
        Self {
            site,
            branch: None,
            call: None,
            occurrence: WorkflowOverlayOccurrence::Unobserved,
            summary: WorkflowOverlaySiteReport::default(),
        }
    }
}

/// Link from an observed parent site to a child execution's overlay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowOverlayChildLink {
    pub parent_execution_key: String,
    pub parent_site: WorkflowSiteRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_execution_key: Option<String>,
    pub child_process_id: lash_sansio::ProcessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_attempt: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_module_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_entry_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_entry_name: Option<String>,
}

/// The actual durable terminal category; abandonment is not a VM failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowOverlayTerminal {
    Completed,
    Failed,
    Cancelled,
    Abandoned,
}

/// Terminal evidence supplied by a committed process fact or durable snapshot.
/// This does not manufacture a language execution or site observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowOverlaySettlement {
    pub terminal: WorkflowOverlayTerminal,
    /// None when the snapshot has no canonical terminal occurrence time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<DateTime<Utc>>,
}
