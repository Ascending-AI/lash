use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::provider::{ProviderWireHeader, ProviderWireRequestMatch};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Matrix {
    pub(super) schema: String,
    pub(super) dialects: Vec<String>,
    pub(super) rows: Vec<MatrixRow>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MatrixRow {
    pub(super) variation: String,
    pub(super) expectation: MatrixExpectation,
    pub(super) cells: BTreeMap<String, MatrixCell>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum MatrixExpectation {
    StopBoundary { literal_present: bool },
    EquivalentOutcomeMapping,
    MissingTerminalEvidence,
    EventShapeVariation,
    ReasoningRenderedPath,
    ResponseIdentityExecutionEvidence,
    MidStreamIdentityConflict,
    MultiResetRetrySequence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum CellDeclarationKind {
    NoReasoningReplay,
    NoResponseTextReplay,
    NoToolReplay,
    NoProviderRequestId,
    FailedResponseInBand,
    BufferedResponseIdentity,
    RetryIdentityFromResponse,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum CellDeclaration {
    NoReasoningReplay { reason: String },
    NoResponseTextReplay { reason: String },
    NoToolReplay { reason: String },
    NoProviderRequestId { reason: String },
    FailedResponseInBand { reason: String },
    BufferedResponseIdentity { reason: String },
    RetryIdentityFromResponse { reason: String },
}

impl CellDeclaration {
    pub(super) fn kind(&self) -> CellDeclarationKind {
        match self {
            Self::NoReasoningReplay { .. } => CellDeclarationKind::NoReasoningReplay,
            Self::NoResponseTextReplay { .. } => CellDeclarationKind::NoResponseTextReplay,
            Self::NoToolReplay { .. } => CellDeclarationKind::NoToolReplay,
            Self::NoProviderRequestId { .. } => CellDeclarationKind::NoProviderRequestId,
            Self::FailedResponseInBand { .. } => CellDeclarationKind::FailedResponseInBand,
            Self::BufferedResponseIdentity { .. } => CellDeclarationKind::BufferedResponseIdentity,
            Self::RetryIdentityFromResponse { .. } => {
                CellDeclarationKind::RetryIdentityFromResponse
            }
        }
    }

    pub(super) fn reason(&self) -> &str {
        match self {
            Self::NoReasoningReplay { reason }
            | Self::NoResponseTextReplay { reason }
            | Self::NoToolReplay { reason }
            | Self::NoProviderRequestId { reason }
            | Self::FailedResponseInBand { reason }
            | Self::BufferedResponseIdentity { reason }
            | Self::RetryIdentityFromResponse { reason } => reason,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum MatrixCell {
    ProviderWireScripts {
        paths: Vec<String>,
        #[serde(default)]
        declarations: Vec<CellDeclaration>,
    },
    RecordedStreams {
        recordings: Vec<Recording>,
        #[serde(default)]
        declarations: Vec<CellDeclaration>,
    },
    NotApplicable {
        reason: String,
        assertion: NotApplicableAssertion,
        recordings: Vec<Recording>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum NotApplicableAssertion {
    OmittedUnsupportedStop,
}

impl MatrixCell {
    pub(super) fn declarations(&self) -> &[CellDeclaration] {
        match self {
            Self::ProviderWireScripts { declarations, .. }
            | Self::RecordedStreams { declarations, .. } => declarations,
            Self::NotApplicable { .. } => &[],
        }
    }

    pub(super) fn declares(&self, kind: CellDeclarationKind) -> bool {
        self.declarations()
            .iter()
            .any(|declaration| declaration.kind() == kind)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Recording {
    pub(super) case: String,
    #[serde(default = "default_http_status")]
    pub(super) status: u16,
    #[serde(default)]
    pub(super) request_match: ProviderWireRequestMatch,
    #[serde(default)]
    pub(super) expected_finish_reason: Option<String>,
    #[serde(default)]
    pub(super) headers: Vec<ProviderWireHeader>,
    #[serde(default)]
    pub(super) events: Vec<RecordedPayload>,
    #[serde(default)]
    pub(super) body: Option<String>,
    #[serde(default)]
    pub(super) close_after_events: bool,
    #[serde(default)]
    pub(super) transport_error: Option<String>,
    #[serde(default)]
    pub(super) retryable: bool,
}

const fn default_http_status() -> u16 {
    200
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecordedPayload {
    #[serde(default)]
    pub(super) json: Option<Value>,
    #[serde(default)]
    pub(super) raw: Option<String>,
}

impl RecordedPayload {
    pub(super) fn wire(&self) -> String {
        match (&self.json, &self.raw) {
            (Some(value), None) => value.to_string(),
            (None, Some(raw)) => raw.clone(),
            _ => panic!("recorded payload must contain exactly one of json or raw"),
        }
    }
}

impl Matrix {
    pub(super) fn load() -> Self {
        serde_json::from_str(super::MATRIX_JSON).expect("provider variation matrix parses")
    }
}
