use std::collections::BTreeMap;

use axum::http::StatusCode;
use lashlang::{GraphRenderError, Span, WorkflowNodeNameSource};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowDocument {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facet_schema_version: Option<u32>,
    pub version: u64,
    pub source: String,
    pub nodes: Vec<FlowNode>,
    pub edges: Vec<FlowEdge>,
    pub roots: GraphRoots,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveWorkflowResponse {
    #[serde(flatten)]
    pub document: WorkflowDocument,
    pub id_map: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProjectWorkflowRequest {
    pub source: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProjectWorkflowResponse {
    pub document: WorkflowDocument,
}

#[derive(Clone, Debug, Serialize)]
pub struct SourceProjectionErrorBody {
    pub error: SourceProjectionError,
}

#[derive(Clone, Debug, Serialize)]
pub struct SourceProjectionError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct SourceProjectionErrorResponse {
    pub(crate) body: SourceProjectionErrorBody,
}

impl SourceProjectionErrorResponse {
    pub(crate) fn invalid_source(message: impl Into<String>) -> Self {
        Self {
            body: SourceProjectionErrorBody {
                error: SourceProjectionError {
                    code: "invalid_source".to_string(),
                    message: message.into(),
                },
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationCatalogEntry {
    pub id: String,
    pub label: String,
    pub node_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subkind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_kind: Option<String>,
    pub fields: Vec<OperationField>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OperationField {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: String,
    pub default: Value,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationKind {
    Expression,
    AssignmentTarget,
    Identifier,
}

impl ValidationKind {
    pub(crate) fn error_code(self) -> &'static str {
        match self {
            Self::Expression => "invalid_expression",
            Self::AssignmentTarget => "invalid_assignment_target",
            Self::Identifier => "invalid_identifier",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ValidateRequest {
    pub kind: ValidationKind,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ValidateResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ValidationError>,
}

impl ValidateResponse {
    pub(crate) fn valid() -> Self {
        Self {
            ok: true,
            error: None,
        }
    }

    pub(crate) fn invalid(kind: ValidationKind, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(ValidationError {
                code: kind.error_code().to_string(),
                message: message.into(),
            }),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ValidationError {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphRoots {
    pub main: Vec<String>,
    #[serde(default)]
    pub processes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowNode {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub data: NodeData,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeData {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subkind: Option<String>,
    #[serde(flatten)]
    pub name: NodeName,
    #[serde(rename = "name", default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<EditableProcessField>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signals: Vec<EditableProcessField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_kind: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, EditableValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterable: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clauses: Vec<EditableComprehensionClause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ChildGroup>,
    #[serde(default)]
    pub available_vars: Vec<TypedVariable>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_arg_types: Vec<ExpectedArgumentType>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<TypeDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedVariable {
    pub name: String,
    #[serde(rename = "type")]
    pub variable_type: String,
}

impl PartialEq<&str> for TypedVariable {
    fn eq(&self, other: &&str) -> bool {
        self.name == *other
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedArgumentType {
    pub slot: String,
    #[serde(rename = "type")]
    pub expected_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeDiagnostic {
    pub node_id: String,
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditableProcessField {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: String,
}

/// A node's display name together with the one fact that decides whether the
/// host may throw it away: who chose it.
///
/// Rendering emits an `@label` annotation only for an authored name, so a
/// payload that carries a title without saying where the name came from is a
/// rename waiting to evaporate. The tag is therefore mandatory and carries the
/// title inside the variant: "title present, tag absent" is unrepresentable,
/// and an unknown tag is a decode error rather than a silent `derived`. The
/// wire values (`label`, `derived`) mirror `WorkflowNodeNameSource`, which the
/// browser client already writes on every node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "nameSource", rename_all = "camelCase")]
pub enum NodeName {
    /// The author named this node: the title and its optional description are
    /// theirs, and the save path renders them back as an `@label` annotation.
    #[serde(rename = "label")]
    Authored {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// The name is a rendering of the node's own expression. The title is
    /// output, not input: the save path recomputes it and it carries no
    /// description.
    Derived { title: String },
}

impl NodeName {
    /// The title to display for this node, authored or rendered.
    pub fn title(&self) -> &str {
        match self {
            Self::Authored { title, .. } | Self::Derived { title } => title,
        }
    }

    /// The authored description, which only an authored name can carry.
    pub fn description(&self) -> Option<&str> {
        match self {
            Self::Authored { description, .. } => description.as_deref(),
            Self::Derived { .. } => None,
        }
    }

    /// The core tag this name projects to.
    pub fn name_source(&self) -> WorkflowNodeNameSource {
        match self {
            Self::Authored { .. } => WorkflowNodeNameSource::Label,
            Self::Derived { .. } => WorkflowNodeNameSource::Derived,
        }
    }

    /// Build the wire name from a projected node's name facts.
    pub fn projected(
        name_source: WorkflowNodeNameSource,
        title: String,
        description: Option<String>,
    ) -> Self {
        match name_source {
            WorkflowNodeNameSource::Label => Self::Authored { title, description },
            WorkflowNodeNameSource::Derived => Self::Derived { title },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EditableComprehensionClause {
    For { binding: String, iterable: String },
    If { condition: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildGroup {
    pub slot: String,
    pub scope: String,
    pub node_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EditableValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    List(Vec<EditableValue>),
    Expr(String),
    Object(BTreeMap<String, EditableValue>),
}

impl Serialize for EditableValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Null => serializer.serialize_none(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => serializer.serialize_f64(*value),
            Self::String(value) => serializer.serialize_str(value),
            Self::List(values) => values.serialize(serializer),
            Self::Expr(source) => BTreeMap::from([("$expr", source)]).serialize(serializer),
            Self::Object(entries) => entries.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for EditableValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_json(Value::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl EditableValue {
    fn from_json(value: Value) -> Result<Self, String> {
        match value {
            Value::Null => Ok(Self::Null),
            Value::Bool(value) => Ok(Self::Bool(value)),
            Value::Number(value) => value
                .as_f64()
                .map(Self::Number)
                .ok_or_else(|| format!("editable number `{value}` is outside the f64 range")),
            Value::String(value) => Ok(Self::String(value)),
            Value::Array(values) => values
                .into_iter()
                .map(Self::from_json)
                .collect::<Result<_, _>>()
                .map(Self::List),
            Value::Object(mut entries)
                if entries.len() == 1 && entries.get("$expr").is_some_and(Value::is_string) =>
            {
                let Value::String(source) = entries.remove("$expr").expect("checked expression")
                else {
                    unreachable!("checked expression value is a string")
                };
                Ok(Self::Expr(source))
            }
            Value::Object(entries) => entries
                .into_iter()
                .map(|(key, value)| Ok((key, Self::from_json(value)?)))
                .collect::<Result<_, String>>()
                .map(Self::Object),
        }
    }
}

#[cfg(test)]
mod editable_value_tests {
    use super::*;

    #[test]
    fn expression_sentinel_round_trips_without_changing_literal_shapes() {
        let expression = EditableValue::Expr("(state.count + 1)".to_string());
        let encoded = serde_json::to_value(&expression).expect("serialize expression value");
        assert_eq!(encoded, json!({ "$expr": "(state.count + 1)" }));
        assert_eq!(
            serde_json::from_value::<EditableValue>(encoded).expect("deserialize expression value"),
            expression
        );

        for (editable, encoded) in [
            (EditableValue::Null, json!(null)),
            (EditableValue::Bool(true), json!(true)),
            (EditableValue::Number(5.0), json!(5.0)),
            (EditableValue::String("s".to_string()), json!("s")),
        ] {
            assert_eq!(
                serde_json::to_value(&editable).expect("serialize literal value"),
                encoded
            );
        }

        let object = json!({ "$expr": "literal member", "other": true });
        assert!(matches!(
            serde_json::from_value::<EditableValue>(object).expect("deserialize object value"),
            EditableValue::Object(entries)
                if entries.len() == 2
                    && entries.get("$expr")
                        == Some(&EditableValue::String("literal member".to_string()))
        ));
    }
}

#[cfg(test)]
mod node_name_tests {
    use super::*;

    fn node_data(name: Value) -> Result<NodeData, serde_json::Error> {
        let mut payload = json!({ "kind": "call" });
        let object = payload.as_object_mut().expect("node data object");
        for (key, value) in name.as_object().expect("name fields") {
            object.insert(key.clone(), value.clone());
        }
        serde_json::from_value::<NodeData>(payload)
    }

    #[test]
    fn an_authored_name_round_trips_with_its_description() {
        let data = node_data(json!({
            "title": "Greet the customer",
            "description": "The opening message",
            "nameSource": "label",
        }))
        .expect("authored node data");

        assert_eq!(
            data.name,
            NodeName::Authored {
                title: "Greet the customer".to_string(),
                description: Some("The opening message".to_string()),
            }
        );
        assert_eq!(data.name.name_source(), WorkflowNodeNameSource::Label);
        assert_eq!(
            serde_json::to_value(&data.name).expect("serialize authored name"),
            json!({
                "nameSource": "label",
                "title": "Greet the customer",
                "description": "The opening message",
            })
        );
    }

    #[test]
    fn a_title_without_a_tag_is_not_representable() {
        let error = node_data(json!({ "title": "Greet the customer" }))
            .expect_err("untagged title must not decode");
        assert!(
            error.to_string().contains("nameSource"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn an_unknown_tag_is_a_decode_error_and_never_derived() {
        let error = node_data(json!({ "title": "Greet the customer", "nameSource": "lable" }))
            .expect_err("unknown tag must not decode");
        let message = error.to_string();
        assert!(
            message.contains("lable") && message.contains("label") && message.contains("derived"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn a_derived_name_carries_no_description() {
        let data = node_data(json!({ "title": "call greet", "nameSource": "derived" }))
            .expect("derived node data");

        assert_eq!(data.name.description(), None);
        assert_eq!(data.name.name_source(), WorkflowNodeNameSource::Derived);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowEdge {
    pub id: String,
    pub source: String,
    pub target: String,
    pub data: EdgeData,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeData {
    pub kind: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variable: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
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

impl DisplayDelta {
    pub(crate) fn merge(&mut self, other: Self) {
        self.messages_appended.extend(other.messages_appended);
        self.statuses.extend(other.statuses);
        for (list, items) in other.list_items_appended {
            self.list_items_appended
                .entry(list)
                .or_default()
                .extend(items);
        }
        self.lights.extend(other.lights);
        if other.progress.is_some() {
            self.progress = other.progress;
        }
        if other.highlighted.is_some() {
            self.highlighted = other.highlighted;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Started,
    Succeeded,
    Waiting,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEvent {
    pub run_id: String,
    pub workflow_version: u64,
    pub sequence: u64,
    pub node_id: String,
    pub status: RunStatus,
    #[serde(default)]
    pub display_delta: DisplayDelta,
    pub display: DisplayState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
pub struct RenderErrorResponse {
    pub(crate) status: StatusCode,
    pub(crate) body: ErrorBody,
}

impl RenderErrorResponse {
    pub(crate) fn render(error: GraphRenderError) -> Self {
        let message = error.to_string();
        let (code, details) = match &error {
            GraphRenderError::UnsupportedSchemaVersion { found, expected } => (
                "unsupported_schema_version",
                json!({ "found": found, "expected": expected }),
            ),
            GraphRenderError::DuplicateNodeId { id } => ("duplicate_node_id", json!({ "id": id })),
            GraphRenderError::UnknownNodeReference {
                edge_id,
                endpoint,
                node_id,
            } => (
                "unknown_node_reference",
                json!({ "edgeId": edge_id, "endpoint": endpoint, "nodeId": node_id }),
            ),
            GraphRenderError::MissingRequiredChild { node_id, child } => (
                "missing_required_child",
                json!({ "nodeId": node_id, "child": child }),
            ),
            GraphRenderError::InvalidNodePayload { node_id, message } => (
                "invalid_node_payload",
                json!({ "nodeId": node_id, "reason": message }),
            ),
            GraphRenderError::InvalidExpression {
                node_id,
                field,
                message,
            } => (
                "invalid_expression",
                json!({ "nodeId": node_id, "field": field, "reason": message }),
            ),
            GraphRenderError::InvalidAssignmentTarget {
                node_id,
                field,
                message,
            } => (
                "invalid_assignment_target",
                json!({ "nodeId": node_id, "field": field, "reason": message }),
            ),
            GraphRenderError::InvalidOpaqueSource { node_id, message } => (
                "invalid_opaque_source",
                json!({ "nodeId": node_id, "reason": message }),
            ),
            GraphRenderError::DuplicateProcessName { name } => {
                ("duplicate_process_name", json!({ "name": name }))
            }
            GraphRenderError::CanonicalSource(_) => ("canonical_source", json!({})),
            GraphRenderError::RenderedSourceInvalid { message } => {
                ("rendered_source_invalid", json!({ "reason": message }))
            }
        };
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code, message, details)
    }

    pub(crate) fn projection(error: lashlang::WorkflowGraphBuildError) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "projection_failed",
            error.to_string(),
            json!({}),
        )
    }

    pub(crate) fn document(message: impl Into<String>, details: Value) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_graph_document",
            message,
            details,
        )
    }

    pub(crate) fn invalid_expression(
        node_id: &str,
        field: &str,
        message: impl Into<String>,
    ) -> Self {
        let message = message.into();
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_expression",
            format!("node `{node_id}` has invalid `{field}` expression text: {message}"),
            json!({ "nodeId": node_id, "field": field, "reason": message }),
        )
    }

    pub(crate) fn invalid_assignment_target(
        node_id: &str,
        field: &'static str,
        message: impl Into<String>,
    ) -> Self {
        let message = message.into();
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_assignment_target",
            format!("node `{node_id}` has invalid `{field}` assignment target text: {message}"),
            json!({ "nodeId": node_id, "field": field, "reason": message }),
        )
    }

    pub(crate) fn invalid_node_payload(node_id: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_node_payload",
            format!("node `{node_id}` has an invalid payload: {message}"),
            json!({ "nodeId": node_id, "reason": message }),
        )
    }

    pub(crate) fn unknown_node_kind(node_id: &str, kind: &str, subkind: Option<&str>) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_node_kind",
            match subkind {
                Some(subkind) => {
                    format!("flow node `{node_id}` has unknown kind `{kind}:{subkind}`")
                }
                None => format!("flow node `{node_id}` has unknown kind `{kind}`"),
            },
            json!({ "nodeId": node_id, "kind": kind, "subkind": subkind }),
        )
    }

    pub(crate) fn run_preparation(message: impl ToString) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "run_preparation_failed",
            message.to_string(),
            json!({}),
        )
    }

    pub(crate) fn version_conflict(submitted: u64, current: u64) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "version_conflict",
            format!("workflow version {submitted} is stale; current version is {current}"),
            json!({ "submitted": submitted, "current": current }),
        )
    }

    pub(crate) fn unknown_workflow(id: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "unknown_workflow",
            format!("workflow example `{id}` does not exist"),
            json!({ "id": id }),
        )
    }

    fn new(
        status: StatusCode,
        code: impl Into<String>,
        message: impl Into<String>,
        details: Value,
    ) -> Self {
        Self {
            status,
            body: ErrorBody {
                error: ErrorDetail {
                    code: code.into(),
                    message: message.into(),
                    details,
                },
            },
        }
    }
}
