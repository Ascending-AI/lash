//! The example's HTTP shapes. Property names of the example's own objects
//! are camelCase; a kernel document, a site, an edit, a correspondence and
//! a workflow view are lash's own serialized shapes, passed through
//! unchanged.

use lash::workflow::document::Correspondence;
use std::collections::BTreeMap;

use axum::http::StatusCode;
use lash::workflow::authoring::Operation;
use lash::workflow::document::{Document, Name, Site, SourceMap};
use lash::workflow::view::{PathStep, WorkflowCoverage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The saved workflow as clients are served it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowView {
    /// The host's optimistic revision of the saved workflow.
    pub version: u64,
    /// The entry of the document a run starts.
    pub entry: Name,
    /// The identity of the draft's document: the base an edit transaction
    /// of this version is applied to.
    pub identity: String,
    /// The definition lash admitted this version as; absent when it did
    /// not, and `notAdmitted` then says why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_admitted: Option<String>,
    /// The kernel document.
    pub document: Document,
    /// The document in kernel notation.
    pub text: String,
    /// Lash's canvas read of the entry, as `lash::workflow::view::derive`
    /// answers it; absent when the document has none, and `viewUnavailable`
    /// then says why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<lash::workflow::view::WorkflowView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_unavailable: Option<String>,
    /// The document as the TypeScript printer spells it: a read-only lens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub source_dialect: Dialect,
    /// Host display data joined by the callee name carried by the view.
    pub function_labels: BTreeMap<String, String>,
    /// Lash's map from the document's sites to byte ranges of `source`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_map: Option<SourceMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_unavailable: Option<String>,
    /// The source this workflow was imported from; absent for a workflow
    /// opened as a kernel document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authored: Option<AuthoredSource>,
}

/// Authored source, the dialect it is written in, and the version
/// importing it saved. Later versions are edits of the imported document;
/// the text is not changed by them.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthoredSource {
    pub dialect: Dialect,
    pub text: String,
    pub version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_map_id: Option<lash::workflow::document::SourceMapId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_map: Option<SourceMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub map_unavailable: Option<String>,
}

/// A dialect a workflow is authored in. Both import through lash's one
/// workflow import under one entry contract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    #[default]
    TypeScript,
    Python,
}

impl Dialect {
    /// Lash's name for the language.
    pub fn language(self) -> lash::workflow::document::SourceLanguage {
        match self {
            Self::TypeScript => lash::workflow::document::SourceLanguage::TypeScript,
            Self::Python => lash::workflow::document::SourceLanguage::Python,
        }
    }
}

/// What this host offers a workflow document.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentView {
    /// Each effect a document may perform, with its signature.
    pub effects: Value,
    /// Each library function by name, with its identity.
    pub functions: BTreeMap<String, String>,
}

/// Open a workflow given as a kernel document.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenWorkflowRequest {
    pub document: Box<Document>,
    pub entry: Name,
}

/// One edit transaction against the saved version.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditWorkflowRequest {
    pub version: u64,
    pub edits: Vec<Operation>,
}

/// The version an edit transaction saved, and where each node of the
/// version it edited is now.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditWorkflowResponse {
    pub workflow: WorkflowView,
    pub correspondence: Correspondence,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayState {
    pub messages: Vec<String>,
    pub statuses: BTreeMap<String, String>,
    pub lists: BTreeMap<String, Vec<String>>,
    pub lights: BTreeMap<String, String>,
    pub progress: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlighted: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayDelta {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages_appended: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub statuses: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub list_items_appended: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lights: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub highlighted: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Started,
    Succeeded,
    Waiting,
    Failed,
    Incomplete,
}

/// Host presentation of a retained tool call. Absence is unavailable evidence.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallData {
    pub input: Value,
    pub output: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEvent {
    pub run_id: String,
    pub workflow_version: u64,
    /// The identity of the document the run executes; the event's site is
    /// a site of that document.
    pub definition: String,
    pub sequence: u64,
    /// Replace the client's provisional observation state before applying this event.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset: bool,
    /// Inspector, display or coverage data that does not update a status.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub call_only: bool,
    /// The site the event is about; absent for the run as a whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<Site>,
    pub status: RunStatus,
    #[serde(default)]
    pub display_delta: DisplayDelta,
    pub display: DisplayState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<lash::ToolCallId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call: Option<CallData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    /// How often each statement and scope of the run's document has run so
    /// far: lash's process overlay read through `workflow::view::coverage`.
    /// Sent when it changes, and on the terminal event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<WorkflowCoverage>,
    /// On the terminal event: the statement path the run took, as lash's
    /// replay verified it, or why it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<RunPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<lash::workflow::view::ActionActivity<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<lash::workflow::document::EffectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_coverage: Option<Vec<lash::workflow::view::NodeCoverage>>,
}

/// A finished run's path through its document: each verified segment's steps
/// through `workflow::view::path`, in segment order.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum RunPath {
    Verified {
        steps: Vec<PathStep>,
    },
    /// A segment did not verify; `reason` is lash's answer for it.
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
    pub details: Value,
}

#[derive(Clone, Debug)]
pub struct ErrorResponse {
    pub status: StatusCode,
    pub body: ErrorBody,
}

impl ErrorResponse {
    pub(crate) fn new(
        status: StatusCode,
        code: &str,
        message: impl std::fmt::Display,
        details: Value,
    ) -> Self {
        Self {
            status,
            body: ErrorBody {
                error: ErrorDetail {
                    code: code.to_owned(),
                    message: message.to_string(),
                    details,
                },
            },
        }
    }

    pub(crate) fn unknown_workflow(id: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_workflow",
            format!("workflow example `{id}` does not exist"),
            serde_json::json!({ "id": id }),
        )
    }

    pub(crate) fn version_conflict(sent: u64, current: u64) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "version_conflict",
            format!("the saved workflow is version {current}, not {sent}"),
            serde_json::json!({ "sent": sent, "current": current }),
        )
    }

    pub(crate) fn invalid(code: &str, message: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code, message, Value::Null)
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunRequest {
    pub version: Option<u64>,
    #[serde(default)]
    pub inputs: serde_json::Map<String, Value>,
}
