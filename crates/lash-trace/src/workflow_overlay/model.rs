use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use lash_sansio::WorkflowSiteRef;
use serde::{Deserialize, Serialize};

use crate::{
    StepBodyStarted, TraceBranchSelection, TraceLanguageChildExecution,
    TraceLanguageExecutionFailure, TraceLanguageExecutionGeneration, TraceLanguageExecutionPayload,
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
    /// The retained observations include the execution's start. Without it
    /// the observer attached late or lost continuity, and occurrences that
    /// ran before are not here.
    pub start_observed: bool,
}

/// What the overlay knows of the document its execution runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorkflowOverlayDocumentBinding {
    /// No document was given and the execution's start was not observed.
    Unknown,
    /// The execution's start named this document; the reducer was not given
    /// it, so the listed sites are whatever was observed.
    Claimed { reference: WorkflowDocumentRef },
    /// The reducer was given this document: every listed site is one of its
    /// sites, and a site it lacks is a mismatch.
    Loaded { reference: WorkflowDocumentRef },
}

impl WorkflowOverlayDocumentBinding {
    /// The document the execution runs, when the overlay knows one.
    pub fn reference(&self) -> Option<&WorkflowDocumentRef> {
        match self {
            Self::Unknown => None,
            Self::Claimed { reference } | Self::Loaded { reference } => Some(reference),
        }
    }

    /// Whether the overlay was held to the document's own sites.
    pub const fn is_loaded(&self) -> bool {
        matches!(self, Self::Loaded { .. })
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

/// Canonical identity of one observation within its execution: two
/// observations with one identity are of the same transition.
///
/// An execution starts once and finishes once. An occurrence starts, ends,
/// selects a branch and starts a child once each, and may wait and resume
/// any number of times: each wait and each resume is its own observation,
/// numbered in time. A step body start is one fact per attempt of its body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "of", rename_all = "snake_case")]
pub enum WorkflowOverlayEventIdentity {
    Execution {
        transition: WorkflowOverlayExecutionTransition,
    },
    Node {
        at: lash_sansio::WorkflowOccurrence,
        transition: WorkflowOverlayNodeTransition,
    },
    StepBody {
        at: lash_sansio::WorkflowOccurrence,
        attempt: u32,
    },
}

impl WorkflowOverlayEventIdentity {
    /// The occurrence the observation is about; `None` for a fact about the
    /// whole execution.
    pub const fn at(&self) -> Option<&lash_sansio::WorkflowOccurrence> {
        match self {
            Self::Execution { .. } => None,
            Self::Node { at, .. } | Self::StepBody { at, .. } => Some(at),
        }
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowOverlayExecutionTransition {
    Started,
    Finished,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowOverlayNodeTransition {
    Started,
    /// The occurrence's `ordinal`-th retained wait, from 1, in time order.
    Waiting {
        ordinal: u32,
    },
    /// The occurrence's `ordinal`-th retained resume, from 1, in time order.
    Resumed {
        ordinal: u32,
    },
    /// The occurrence completed, failed or was cancelled.
    Terminal,
    BranchSelected,
    ChildStarted,
}

/// One observation the overlay folds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "fact", rename_all = "snake_case")]
pub enum WorkflowOverlayFact {
    /// What the language execution reported.
    Language {
        document: Box<WorkflowDocumentRef>,
        payload: TraceLanguageExecutionPayload,
    },
    /// The admitted body of a process step started.
    StepBodyStarted { step: StepBodyStarted },
}

/// One canonical observation retained so a persisted overlay can be folded
/// again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowOverlayHistoryEvent {
    pub identity: WorkflowOverlayEventIdentity,
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub fact: WorkflowOverlayFact,
}

/// Why two records under one logical identity did not deduplicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowOverlayConflictKind {
    ConflictingDuplicate,
}

/// A bounded, typed record of divergent observations under one identity.
/// The overlay keeps the earlier observation; of two at one instant it keeps
/// the one whose variant digest is smaller. `variants` lists the smallest and
/// the largest digest observed.
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
    pub scope: TraceRuntimeScope,
    pub subject: TraceRuntimeSubject,
    #[serde(flatten)]
    pub generation: Option<TraceLanguageExecutionGeneration>,
    pub document: WorkflowOverlayDocumentBinding,
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
    scope: TraceRuntimeScope,
    subject: TraceRuntimeSubject,
    attempt: Option<u32>,
    document: WorkflowOverlayDocumentBinding,
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

impl WorkflowExecutionOverlay {
    /// The execution's key: its subject and, for an attempted execution, its
    /// attempt. Two runs of one document never share it.
    pub fn execution_key(&self) -> String {
        execution_key(&self.subject, self.generation)
    }

    /// Whether the overlay covers the execution from its start against its
    /// document. Per-site truncation is reported by the retention records.
    pub const fn is_complete(&self) -> bool {
        self.document.is_loaded() && self.coverage.start_observed
    }
}

pub(super) fn execution_key(
    subject: &TraceRuntimeSubject,
    generation: Option<TraceLanguageExecutionGeneration>,
) -> String {
    match generation {
        Some(generation) => format!(
            "{}:attempt:{attempt}",
            subject.graph_key(),
            attempt = generation.attempt(),
        ),
        None => subject.graph_key(),
    }
}

/// What one site keeps of the occurrences evicted from its bounded history.
///
/// Occurrences at or below `truncation_watermark` are no longer in the
/// overlay's history. The watermark occurrence keeps its observations, so
/// one that arrives late still folds exactly. The occurrences below it are
/// folded into `archived`; of those, a late observation can refine only the
/// occurrence `archived` shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowOverlaySiteRetention {
    pub site: WorkflowSiteRef,
    pub truncation_watermark: u64,
    /// What the occurrences below the watermark folded to.
    pub archived: WorkflowOverlaySiteState,
    /// The child executions the occurrences below the watermark started.
    pub archived_children: Vec<TraceLanguageChildExecution>,
    /// The observations of the watermark occurrence.
    pub watermark_history: Vec<WorkflowOverlayHistoryEvent>,
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
    },
    Failed {
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<DateTime<Utc>>,
        end: DateTime<Utc>,
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

    /// How long a completed or failed occurrence ran, when its start was
    /// observed.
    pub fn duration_ms(&self) -> Option<i64> {
        match self {
            Self::Completed {
                start: Some(start),
                end,
                ..
            }
            | Self::Failed {
                start: Some(start),
                end,
                ..
            } => Some(end.signed_duration_since(start).num_milliseconds().max(0)),
            _ => None,
        }
    }

    /// When the occurrence was first observed to start.
    pub const fn start(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Unobserved => None,
            Self::Running { start, .. } => Some(*start),
            Self::Waiting { start, .. }
            | Self::Completed { start, .. }
            | Self::Failed { start, .. }
            | Self::Cancelled { start, .. }
            | Self::Incomplete { start, .. } => *start,
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
    #[serde(flatten)]
    pub state: WorkflowOverlaySiteState,
}

/// What a site's occurrences fold to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowOverlaySiteState {
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

/// Link from an observed parent site to a child execution's overlay. The
/// parent execution is the overlay's own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowOverlayChildLink {
    pub parent_site: WorkflowSiteRef,
    pub child: TraceLanguageChildExecution,
}

impl WorkflowOverlayChildLink {
    /// The child execution's key, when its attempt is known.
    pub fn child_execution_key(&self) -> Option<String> {
        self.child.graph_key()
    }
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
