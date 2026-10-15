//! Which kinds lash builds, which the exporter exports and which metric
//! families it records. Building and exporting are separate policies: a kind
//! lash builds but does not export still reaches every sink.

use serde::Serialize;

use crate::{TraceSpanKind, TraceSpanKindSet};

use super::{PolicyOperand, PolicyRelation, PolicyValue, TracePolicyError, TracePolicyField};
use super::{TracePolicySelection, check_off_rule};

/// How much lash builds for one span kind, for every consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceDetail {
    /// Nothing: no record of the kind is built.
    Off,
    /// The kind's records.
    Standard,
    /// The kind's records and its extended detail: provider and stream event
    /// records for a model call, node spans for a workflow.
    Extended,
}

impl TraceDetail {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Standard => "standard",
            Self::Extended => "extended",
        }
    }

    /// Whether the kind is built at all.
    pub const fn is_on(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub const fn is_extended(self) -> bool {
        matches!(self, Self::Extended)
    }
}

/// What lash builds for each span kind: one [`TraceDetail`] per kind.
///
/// A container (producer, turn, tool, cell, process, workflow, node, tool
/// intent) cannot be off, and only a model call and a workflow have
/// extended detail. A leaf can be off only with every kind it can parent:
/// a model call with its attempts, an extension span with points, model
/// calls and their attempts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceDetailPolicy {
    details: [TraceDetail; TraceSpanKind::COUNT],
}

impl TraceDetailPolicy {
    /// Every kind `Standard`; a workflow node follows its workflow.
    pub const fn standard() -> Self {
        Self {
            details: [TraceDetail::Standard; TraceSpanKind::COUNT],
        }
    }

    /// [`standard`](Self::standard) with model calls `Extended`: their
    /// provider and stream event records reach every sink.
    pub const fn extended_models() -> Self {
        let mut details = [TraceDetail::Standard; TraceSpanKind::COUNT];
        details[TraceSpanKind::Model.index()] = TraceDetail::Extended;
        Self { details }
    }

    pub fn builder() -> TraceDetailPolicyBuilder {
        TraceDetailPolicyBuilder {
            policy: Self::standard(),
        }
    }

    /// The detail of `kind`. A workflow node follows its workflow: its
    /// spans exist when the workflow's detail is `Extended`.
    pub fn get(&self, kind: TraceSpanKind) -> TraceDetail {
        self.details[kind.index()]
    }

    /// Whether lash builds `kind` at all.
    pub fn builds(&self, kind: TraceSpanKind) -> bool {
        self.get(kind).is_on()
    }
}

impl Default for TraceDetailPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

impl Serialize for TraceDetailPolicy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(
            TraceSpanKind::ALL
                .into_iter()
                .map(|kind| (kind.as_str(), self.get(kind))),
        )
    }
}

pub struct TraceDetailPolicyBuilder {
    policy: TraceDetailPolicy,
}

impl TraceDetailPolicyBuilder {
    #[must_use]
    pub fn set(mut self, kind: TraceSpanKind, detail: TraceDetail) -> Self {
        self.policy.details[kind.index()] = detail;
        self
    }

    /// Refuses, in kind order, a container turned off, extended detail for a
    /// kind that has none, and any workflow node setting but `Standard`
    /// (`Structural`); then a leaf turned off while a kind it can parent is
    /// on (`Relation`, `ParentOffChildOn`).
    pub fn build(self) -> Result<TraceDetailPolicy, TracePolicyError> {
        let policy = self.policy;
        for kind in TraceSpanKind::ALL {
            let detail = policy.get(kind);
            let field = TracePolicyField::Detail(kind);
            let refused = match (kind, detail) {
                (TraceSpanKind::WorkflowNode, TraceDetail::Off) => Some(TracePolicySelection::Off),
                (TraceSpanKind::WorkflowNode, TraceDetail::Extended) => {
                    Some(TracePolicySelection::Extended)
                }
                (_, TraceDetail::Off) if kind.is_container() => Some(TracePolicySelection::Off),
                (TraceSpanKind::Model | TraceSpanKind::Workflow, _) => None,
                (_, TraceDetail::Extended) => Some(TracePolicySelection::Extended),
                _ => None,
            };
            if let Some(selection) = refused {
                return Err(TracePolicyError::Structural { field, selection });
            }
        }
        check_off_rule(
            |kind| !policy.builds(kind),
            |kind| {
                (
                    TracePolicyField::Detail(kind),
                    PolicyValue::Detail(policy.get(kind)),
                )
            },
        )?;
        Ok(policy)
    }
}

/// Which kinds the OpenTelemetry exporter turns into spans. A skipped kind's
/// records still reach every sink, and its metrics are still recorded.
///
/// A container is always exported, and a leaf may be skipped only with every
/// kind it can parent, so no exported span names a parent that was not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceExportPolicy {
    skipped: TraceSpanKindSet,
}

impl TraceExportPolicy {
    /// Every kind exported.
    pub const fn standard() -> Self {
        Self {
            skipped: TraceSpanKindSet::EMPTY,
        }
    }

    pub fn exports(&self, kind: TraceSpanKind) -> bool {
        !self.skipped.contains(kind)
    }

    pub fn builder() -> TraceExportPolicyBuilder {
        TraceExportPolicyBuilder {
            skipped: Vec::new(),
        }
    }
}

impl Default for TraceExportPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

impl Serialize for TraceExportPolicy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(
            TraceSpanKind::ALL
                .into_iter()
                .map(|kind| (kind.as_str(), self.exports(kind))),
        )
    }
}

pub struct TraceExportPolicyBuilder {
    skipped: Vec<TraceSpanKind>,
}

impl TraceExportPolicyBuilder {
    #[must_use]
    pub fn skip(mut self, kind: TraceSpanKind) -> Self {
        self.skipped.push(kind);
        self
    }

    /// Refuses a skipped container (`Structural`), then a leaf skipped while
    /// a kind it can parent is exported (`Relation`, `ParentOffChildOn`).
    pub fn build(self) -> Result<TraceExportPolicy, TracePolicyError> {
        let policy = TraceExportPolicy {
            skipped: TraceSpanKindSet::of(&self.skipped),
        };
        if let Some(kind) = TraceSpanKind::ALL
            .into_iter()
            .find(|kind| kind.is_container() && !policy.exports(*kind))
        {
            return Err(TracePolicyError::Structural {
                field: TracePolicyField::Export(kind),
                selection: TracePolicySelection::Off,
            });
        }
        check_off_rule(
            |kind| !policy.exports(kind),
            |kind| {
                (
                    TracePolicyField::Export(kind),
                    PolicyValue::Exported(policy.exports(kind)),
                )
            },
        )?;
        Ok(policy)
    }
}

/// Which metric families the exporter records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceMetrics {
    /// No instruments.
    Off,
    /// The runtime's infrastructure instruments only.
    Runtime,
    /// The GenAI semantic-convention instruments only.
    GenAi,
    #[default]
    All,
}

impl TraceMetrics {
    /// Whether the runtime's infrastructure instruments are recorded.
    pub const fn records_runtime(self) -> bool {
        matches!(self, Self::Runtime | Self::All)
    }

    /// Whether the GenAI instruments are recorded.
    pub const fn records_gen_ai(self) -> bool {
        matches!(self, Self::GenAi | Self::All)
    }
}

/// The off rule: a kind that is off (`off(kind)`) while a kind in its
/// [`children`](TraceSpanKind::children) is on is refused, naming both
/// sides as `operand(kind)` gives them.
pub(super) fn off_rule_relation(
    parent: TraceSpanKind,
    child: TraceSpanKind,
    parent_side: (TracePolicyField, PolicyValue),
    child_side: (TracePolicyField, PolicyValue),
) -> TracePolicyError {
    debug_assert!(parent.children().contains(child));
    TracePolicyError::Relation {
        field: parent_side.0,
        value: parent_side.1,
        relation: PolicyRelation::ParentOffChildOn,
        other: PolicyOperand::Field(child_side.0),
        other_value: child_side.1,
    }
}
