use std::collections::BTreeMap;

use axum::http::StatusCode;
use lash::rlm::lang::{
    Span, WorkflowDiagnosticClassification, WorkflowEdgeKind, WorkflowEffectKind,
    WorkflowNodeNameSource, WorkflowTerminalKind,
};
use lash::typescript::workflow_graph::{GraphRenderError, WorkflowGraphBuildError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowDocument {
    pub schema_version: u32,
    /// The source identity of the admitted artifact this document's graph
    /// is the view of; absent for a draft whose source does not admit. A run
    /// overlay shows a run's events only when their `definition` is this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
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
    /// The host receiver the operation is reached through (`display`, `gmail`,
    /// `llm`, …). A call entry's synthesized expression is
    /// `await {receiver}.{operation}({..})`, and both the editor and the
    /// backend fallback read the receiver from here rather than assuming
    /// `display` (FIG-3178).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver: Option<String>,
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
    pub default: EditableValue,
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
            Self::Expression => GraphRenderError::InvalidExpression {
                node_id: String::new(),
                field: String::new(),
                message: String::new(),
            }
            .code(),
            Self::AssignmentTarget => GraphRenderError::InvalidAssignmentTarget {
                node_id: String::new(),
                field: "target",
                message: String::new(),
            }
            .code(),
            Self::Identifier => "invalid_identifier",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidateRequest {
    pub kind: ValidationKind,
    pub text: String,
    /// The identifiers live where the fragment sits. A TypeScript fragment is
    /// parsed by the dialect's own front-end, which rejects an unknown binding,
    /// so the editor sends the node's available variables with the text.
    #[serde(default)]
    pub available_vars: Vec<String>,
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

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GraphRoots {
    pub main: Vec<String>,
    #[serde(default)]
    pub processes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlowNode {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub data: NodeData,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(transform = close_flattened_union)]
pub struct NodeData {
    #[serde(flatten)]
    pub name: NodeName,
    #[serde(default)]
    pub available_vars: Vec<TypedVariable>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_arg_types: Vec<ExpectedArgumentType>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<TypeDiagnostic>,
    #[serde(flatten)]
    pub body: NodeBody,
}

impl<'de> Deserialize<'de> for NodeData {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Envelope {
            #[serde(default)]
            available_vars: Vec<TypedVariable>,
            #[serde(default)]
            expected_arg_types: Vec<ExpectedArgumentType>,
            #[serde(default)]
            diagnostics: Vec<TypeDiagnostic>,
            #[serde(flatten)]
            body: BTreeMap<String, Value>,
        }
        let mut payload = BTreeMap::<String, Value>::deserialize(deserializer)?;
        let name = Value::Object(
            ["nameSource", "title", "description"]
                .into_iter()
                .filter_map(|field| payload.remove(field).map(|value| (field.to_owned(), value)))
                .collect(),
        );
        let name = serde_json::from_value(name).map_err(serde::de::Error::custom)?;
        let envelope: Envelope =
            serde_json::from_value(Value::Object(payload.into_iter().collect()))
                .map_err(serde::de::Error::custom)?;
        let body = serde_json::from_value(Value::Object(envelope.body.into_iter().collect()))
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            name,
            available_vars: envelope.available_vars,
            expected_arg_types: envelope.expected_arg_types,
            diagnostics: envelope.diagnostics,
            body,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum NodeBody {
    Process {
        #[serde(rename = "name")]
        #[serde(default, skip_serializing_if = "Option::is_none")]
        process_name: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        params: Vec<EditableProcessField>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        children: Vec<ChildGroup>,
    },
    Data {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expression: Option<String>,
        #[serde(default)]
        fields: BTreeMap<String, EditableValue>,
    },
    Call {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receiver: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expression: Option<String>,
        #[serde(default)]
        fields: BTreeMap<String, EditableValue>,
    },
    Effect {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        effect: WorkflowEffectKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expression: Option<String>,
        #[serde(default)]
        fields: BTreeMap<String, EditableValue>,
    },
    StateUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expression: Option<String>,
        #[serde(default)]
        fields: BTreeMap<String, EditableValue>,
    },
    Computation {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expression: Option<String>,
        #[serde(default)]
        fields: BTreeMap<String, EditableValue>,
    },
    Terminal {
        terminal_kind: WorkflowTerminalKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expression: Option<String>,
    },
    Opaque {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
    Container(NodeContainer),
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "subkind", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeContainer {
    If {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        condition: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        children: Vec<ChildGroup>,
    },
    While {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        condition: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        children: Vec<ChildGroup>,
    },
    For {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iterable: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        children: Vec<ChildGroup>,
    },
}

impl NodeData {
    pub fn kind(&self) -> &'static str {
        match &self.body {
            NodeBody::Process { .. } => "process",
            NodeBody::Data { .. } => "data",
            NodeBody::Call { .. } => "call",
            NodeBody::Effect { .. } => "effect",
            NodeBody::StateUpdate { .. } => "state_update",
            NodeBody::Computation { .. } => "computation",
            NodeBody::Terminal { .. } => "terminal",
            NodeBody::Opaque { .. } => "opaque",
            NodeBody::Container(_) => "container",
        }
    }
    pub fn subkind(&self) -> Option<&'static str> {
        match &self.body {
            NodeBody::Container(NodeContainer::If { .. }) => Some("if"),
            NodeBody::Container(NodeContainer::While { .. }) => Some("while"),
            NodeBody::Container(NodeContainer::For { .. }) => Some("for"),
            _ => None,
        }
    }
    pub fn process_name(&self) -> &Option<String> {
        match &self.body {
            NodeBody::Process { process_name, .. } => process_name,
            _ => &None,
        }
    }
    pub fn process_name_mut(&mut self) -> Option<&mut Option<String>> {
        match &mut self.body {
            NodeBody::Process { process_name, .. } => Some(process_name),
            _ => None,
        }
    }
    pub fn params(&self) -> &Vec<EditableProcessField> {
        match &self.body {
            NodeBody::Process { params, .. } => params,
            _ => {
                static EMPTY: std::sync::LazyLock<Vec<EditableProcessField>> =
                    std::sync::LazyLock::new(Vec::new);
                &EMPTY
            }
        }
    }
    pub fn params_mut(&mut self) -> Option<&mut Vec<EditableProcessField>> {
        match &mut self.body {
            NodeBody::Process { params, .. } => Some(params),
            _ => None,
        }
    }
    pub fn children(&self) -> &Vec<ChildGroup> {
        match &self.body {
            NodeBody::Process { children, .. } => children,
            NodeBody::Container(NodeContainer::If { children, .. }) => children,
            NodeBody::Container(NodeContainer::While { children, .. }) => children,
            NodeBody::Container(NodeContainer::For { children, .. }) => children,
            _ => {
                static EMPTY: std::sync::LazyLock<Vec<ChildGroup>> =
                    std::sync::LazyLock::new(Vec::new);
                &EMPTY
            }
        }
    }
    pub fn children_mut(&mut self) -> Option<&mut Vec<ChildGroup>> {
        match &mut self.body {
            NodeBody::Process { children, .. } => Some(children),
            NodeBody::Container(NodeContainer::If { children, .. }) => Some(children),
            NodeBody::Container(NodeContainer::While { children, .. }) => Some(children),
            NodeBody::Container(NodeContainer::For { children, .. }) => Some(children),
            _ => None,
        }
    }
    pub fn binding(&self) -> &Option<String> {
        match &self.body {
            NodeBody::Data { binding, .. } => binding,
            NodeBody::Call { binding, .. } => binding,
            NodeBody::Effect { binding, .. } => binding,
            NodeBody::Computation { binding, .. } => binding,
            NodeBody::Container(NodeContainer::If { binding, .. }) => binding,
            NodeBody::Container(NodeContainer::For { binding, .. }) => binding,
            _ => &None,
        }
    }
    pub fn binding_mut(&mut self) -> Option<&mut Option<String>> {
        match &mut self.body {
            NodeBody::Data { binding, .. } => Some(binding),
            NodeBody::Call { binding, .. } => Some(binding),
            NodeBody::Effect { binding, .. } => Some(binding),
            NodeBody::Computation { binding, .. } => Some(binding),
            NodeBody::Container(NodeContainer::If { binding, .. }) => Some(binding),
            NodeBody::Container(NodeContainer::For { binding, .. }) => Some(binding),
            _ => None,
        }
    }
    pub fn expression(&self) -> &Option<String> {
        match &self.body {
            NodeBody::Data { expression, .. } => expression,
            NodeBody::Call { expression, .. } => expression,
            NodeBody::Effect { expression, .. } => expression,
            NodeBody::StateUpdate { expression, .. } => expression,
            NodeBody::Computation { expression, .. } => expression,
            NodeBody::Terminal { expression, .. } => expression,
            _ => &None,
        }
    }
    pub fn expression_mut(&mut self) -> Option<&mut Option<String>> {
        match &mut self.body {
            NodeBody::Data { expression, .. } => Some(expression),
            NodeBody::Call { expression, .. } => Some(expression),
            NodeBody::Effect { expression, .. } => Some(expression),
            NodeBody::StateUpdate { expression, .. } => Some(expression),
            NodeBody::Computation { expression, .. } => Some(expression),
            NodeBody::Terminal { expression, .. } => Some(expression),
            _ => None,
        }
    }
    pub fn fields(&self) -> &BTreeMap<String, EditableValue> {
        match &self.body {
            NodeBody::Data { fields, .. } => fields,
            NodeBody::Call { fields, .. } => fields,
            NodeBody::Effect { fields, .. } => fields,
            NodeBody::StateUpdate { fields, .. } => fields,
            NodeBody::Computation { fields, .. } => fields,
            _ => {
                static EMPTY: std::sync::LazyLock<BTreeMap<String, EditableValue>> =
                    std::sync::LazyLock::new(BTreeMap::new);
                &EMPTY
            }
        }
    }
    pub fn fields_mut(&mut self) -> Option<&mut BTreeMap<String, EditableValue>> {
        match &mut self.body {
            NodeBody::Data { fields, .. } => Some(fields),
            NodeBody::Call { fields, .. } => Some(fields),
            NodeBody::Effect { fields, .. } => Some(fields),
            NodeBody::StateUpdate { fields, .. } => Some(fields),
            NodeBody::Computation { fields, .. } => Some(fields),
            _ => None,
        }
    }
    pub fn operation(&self) -> &Option<String> {
        match &self.body {
            NodeBody::Call { operation, .. } => operation,
            _ => &None,
        }
    }
    pub fn operation_mut(&mut self) -> Option<&mut Option<String>> {
        match &mut self.body {
            NodeBody::Call { operation, .. } => Some(operation),
            _ => None,
        }
    }
    pub fn receiver(&self) -> &Option<String> {
        match &self.body {
            NodeBody::Call { receiver, .. } => receiver,
            _ => &None,
        }
    }
    pub fn receiver_mut(&mut self) -> Option<&mut Option<String>> {
        match &mut self.body {
            NodeBody::Call { receiver, .. } => Some(receiver),
            _ => None,
        }
    }
    pub fn target(&self) -> &Option<String> {
        match &self.body {
            NodeBody::StateUpdate { target, .. } => target,
            _ => &None,
        }
    }
    pub fn target_mut(&mut self) -> Option<&mut Option<String>> {
        match &mut self.body {
            NodeBody::StateUpdate { target, .. } => Some(target),
            _ => None,
        }
    }
    pub fn source(&self) -> &Option<String> {
        match &self.body {
            NodeBody::Opaque { source, .. } => source,
            _ => &None,
        }
    }
    pub fn source_mut(&mut self) -> Option<&mut Option<String>> {
        match &mut self.body {
            NodeBody::Opaque { source, .. } => Some(source),
            _ => None,
        }
    }
    pub fn condition(&self) -> &Option<String> {
        match &self.body {
            NodeBody::Container(NodeContainer::If { condition, .. }) => condition,
            NodeBody::Container(NodeContainer::While { condition, .. }) => condition,
            _ => &None,
        }
    }
    pub fn condition_mut(&mut self) -> Option<&mut Option<String>> {
        match &mut self.body {
            NodeBody::Container(NodeContainer::If { condition, .. }) => Some(condition),
            NodeBody::Container(NodeContainer::While { condition, .. }) => Some(condition),
            _ => None,
        }
    }
    pub fn iterable(&self) -> &Option<String> {
        match &self.body {
            NodeBody::Container(NodeContainer::For { iterable, .. }) => iterable,
            _ => &None,
        }
    }
    pub fn iterable_mut(&mut self) -> Option<&mut Option<String>> {
        match &mut self.body {
            NodeBody::Container(NodeContainer::For { iterable, .. }) => Some(iterable),
            _ => None,
        }
    }
    pub fn effect(&self) -> Option<WorkflowEffectKind> {
        match self.body {
            NodeBody::Effect { effect, .. } => Some(effect),
            _ => None,
        }
    }
    pub fn effect_mut(&mut self) -> Option<&mut WorkflowEffectKind> {
        match &mut self.body {
            NodeBody::Effect { effect, .. } => Some(effect),
            _ => None,
        }
    }
    pub fn terminal_kind(&self) -> Option<&WorkflowTerminalKind> {
        match &self.body {
            NodeBody::Terminal { terminal_kind, .. } => Some(terminal_kind),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExpectedArgumentType {
    pub slot: String,
    #[serde(rename = "type")]
    pub expected_type: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TypeDiagnostic {
    pub node_id: String,
    pub kind: String,
    pub classification: WorkflowDiagnosticClassification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "nameSource", rename_all = "camelCase", deny_unknown_fields)]
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

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChildGroup {
    pub slot: String,
    pub scope: String,
    pub node_ids: Vec<String>,
}

/// An editable field carries its authored kind at every depth. Literal keys
/// are data, including `$expr`, `kind` and `value`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum EditableValue {
    Null(()),
    Bool(bool),
    Number(f64),
    String(String),
    List(Vec<EditableValue>),
    Expr(String),
    Object(BTreeMap<String, EditableValue>),
}

#[cfg(test)]
mod editable_value_tests {
    use super::*;

    #[test]
    fn nested_literal_records_keep_arbitrary_keys_and_expression_kinds() {
        let value = EditableValue::List(vec![
            EditableValue::Object(BTreeMap::from([
                (
                    "$expr".to_string(),
                    EditableValue::String("1 + 1".to_string()),
                ),
                (
                    "kind".to_string(),
                    EditableValue::String("expr".to_string()),
                ),
                (
                    "value".to_string(),
                    EditableValue::Object(BTreeMap::from([(
                        "$expr".to_string(),
                        EditableValue::String("not valid code!".to_string()),
                    )])),
                ),
            ])),
            EditableValue::Expr("1 + 1".to_string()),
        ]);
        let bytes = serde_json::to_vec(&value).expect("encode nested values");
        assert_eq!(
            serde_json::from_slice::<EditableValue>(&bytes).expect("decode nested values"),
            value
        );
    }

    #[test]
    fn every_variant_uses_explicit_kind_and_value() {
        for (editable, encoded) in [
            (
                EditableValue::Null(()),
                json!({ "kind": "null", "value": null }),
            ),
            (
                EditableValue::Bool(true),
                json!({ "kind": "bool", "value": true }),
            ),
            (
                EditableValue::Number(5.0),
                json!({ "kind": "number", "value": 5.0 }),
            ),
            (
                EditableValue::String("s".to_string()),
                json!({ "kind": "string", "value": "s" }),
            ),
            (
                EditableValue::Expr("1 + 1".to_string()),
                json!({ "kind": "expr", "value": "1 + 1" }),
            ),
            (
                EditableValue::List(vec![EditableValue::Null(())]),
                json!({ "kind": "list", "value": [{ "kind": "null", "value": null }] }),
            ),
            (
                EditableValue::Object(BTreeMap::from([(
                    "$expr".to_string(),
                    EditableValue::String("1 + 1".to_string()),
                )])),
                json!({ "kind": "object", "value": { "$expr": { "kind": "string", "value": "1 + 1" } } }),
            ),
        ] {
            assert_eq!(
                serde_json::to_value(&editable).expect("encode value"),
                encoded
            );
            assert_eq!(
                serde_json::from_value::<EditableValue>(encoded).expect("decode value"),
                editable
            );
        }
    }

    #[test]
    fn untagged_or_malformed_values_are_rejected() {
        for value in [
            json!({ "$expr": "1 + 1" }),
            json!("text"),
            json!([1]),
            json!({ "kind": "expression", "value": "1 + 1" }),
            json!({ "kind": "expr", "value": { "$expr": "1 + 1" } }),
            json!({ "kind": "object", "value": { "$expr": "1 + 1" } }),
            json!({ "kind": "list", "value": [true] }),
        ] {
            assert!(
                serde_json::from_value::<EditableValue>(value.clone()).is_err(),
                "accepted {value}"
            );
        }
    }
}

#[cfg(test)]
mod node_name_tests {
    use super::*;

    #[test]
    fn node_kinds_reject_foreign_payloads_and_unknown_discriminants() {
        for body in [
            json!({"kind":"process"}),
            json!({"kind":"data"}),
            json!({"kind":"call"}),
            json!({"kind":"effect","effect":"sleep_for"}),
            json!({"kind":"state_update"}),
            json!({"kind":"computation"}),
            json!({"kind":"terminal","terminalKind":"finish"}),
            json!({"kind":"opaque"}),
            json!({"kind":"container","subkind":"if"}),
            json!({"kind":"container","subkind":"while"}),
            json!({"kind":"container","subkind":"for"}),
        ] {
            let mut payload = body;
            payload["title"] = json!("node");
            payload["nameSource"] = json!("derived");
            let decoded: NodeData = serde_json::from_value(payload.clone()).expect("valid node");
            let round: NodeData =
                serde_json::from_value(serde_json::to_value(&decoded).expect("encode node"))
                    .expect("round trip");
            assert_eq!(round.kind(), payload["kind"].as_str().expect("kind"));
        }
        for body in [
            json!({"kind": "cal", "operation": "greet"}),
            json!({"kind": "call", "operation": "greet", "condition": "true"}),
            json!({"kind": "data", "expression": "1", "effect": "print"}),
            json!({"kind": "terminal", "expression": "1", "terminalKind": "finsh"}),
            json!({"kind": "effect", "effect": "sleep"}),
            json!({"kind": "container", "subkind": "whlie"}),
        ] {
            let mut payload = body;
            payload["title"] = json!("node");
            payload["nameSource"] = json!("derived");
            assert!(
                serde_json::from_value::<NodeData>(payload.clone()).is_err(),
                "invalid node decoded: {payload}"
            );
        }
    }

    #[test]
    fn edge_kinds_reject_control_data_and_incomplete_versions() {
        for payload in [
            json!({"kind":"sequence","scope":"main"}),
            json!({"kind":"data_dependency","scope":"main","variable":"x","version":1}),
        ] {
            let decoded: EdgeData = serde_json::from_value(payload.clone()).expect("valid edge");
            assert_eq!(serde_json::to_value(decoded).expect("encode edge"), payload);
        }
        for payload in [
            json!({"kind": "sequence", "scope": "main", "variable":"x", "version":1}),
            json!({"kind": "data_dependency", "scope":"main", "variable":"x"}),
            json!({"kind": "contrl", "scope": "main"}),
            json!({"kind": "control", "scope": "main", "variable": "x", "version": 1}),
            json!({"kind": "data", "scope": "main", "variable": "x"}),
        ] {
            assert!(
                serde_json::from_value::<EdgeData>(payload.clone()).is_err(),
                "invalid edge decoded: {payload}"
            );
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlowEdge {
    pub id: String,
    pub source: String,
    pub target: String,
    pub data: EdgeData,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(transform = close_flattened_union)]
pub struct EdgeData {
    #[serde(flatten)]
    pub body: WorkflowEdgeKind,
    pub scope: String,
}

impl<'de> Deserialize<'de> for EdgeData {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut payload = BTreeMap::<String, Value>::deserialize(deserializer)?;
        let scope = payload
            .remove("scope")
            .ok_or_else(|| serde::de::Error::missing_field("scope"))?;
        let scope = serde_json::from_value(scope).map_err(serde::de::Error::custom)?;
        let body: WorkflowEdgeKind =
            serde_json::from_value(Value::Object(payload.clone().into_iter().collect()))
                .map_err(serde::de::Error::custom)?;
        let canonical = serde_json::to_value(&body).map_err(serde::de::Error::custom)?;
        if let Some(field) = payload.keys().find(|field| canonical.get(*field).is_none()) {
            return Err(serde::de::Error::custom(format!(
                "unknown edge field `{field}`"
            )));
        }
        Ok(Self { body, scope })
    }
}

/// Flatten the generated intersections into closed object variants, so schema
/// validators and the TypeScript compiler enforce the same owned payloads.
#[expect(
    clippy::expect_used,
    reason = "the derive emits inline object branches for these flattened tagged unions"
)]
fn close_flattened_union(schema: &mut schemars::Schema) {
    fn merge(
        mut left: serde_json::Map<String, Value>,
        right: serde_json::Map<String, Value>,
    ) -> serde_json::Map<String, Value> {
        for (key, value) in right {
            match (key.as_str(), left.get_mut(&key), value) {
                ("properties", Some(Value::Object(current)), Value::Object(properties)) => {
                    current.extend(properties)
                }
                ("required", Some(Value::Array(current)), Value::Array(required)) => {
                    current.extend(required)
                }
                (_, _, value) => {
                    left.insert(key, value);
                }
            }
        }
        left
    }
    fn variants(mut object: serde_json::Map<String, Value>) -> Vec<serde_json::Map<String, Value>> {
        let alternatives = object.remove("oneOf");
        let intersections = object.remove("allOf");
        object.remove("additionalProperties");
        object.remove("unevaluatedProperties");
        let mut result = vec![object];
        if let Some(Value::Array(alternatives)) = alternatives {
            result = result
                .into_iter()
                .flat_map(|base| {
                    alternatives.iter().flat_map(move |alternative| {
                        variants(
                            alternative
                                .as_object()
                                .expect("object union variant")
                                .clone(),
                        )
                        .into_iter()
                        .map({
                            let base = base.clone();
                            move |variant| merge(base.clone(), variant)
                        })
                    })
                })
                .collect();
        }
        if let Some(Value::Array(intersections)) = intersections {
            for intersection in intersections {
                let members = variants(
                    intersection
                        .as_object()
                        .expect("object intersection")
                        .clone(),
                );
                result = result
                    .into_iter()
                    .flat_map(|base| {
                        members
                            .iter()
                            .cloned()
                            .map(move |member| merge(base.clone(), member))
                    })
                    .collect();
            }
        }
        result
    }
    let object = schema.as_object().expect("flattened object schema").clone();
    let variants = variants(object)
        .into_iter()
        .map(|mut variant| {
            variant.insert("additionalProperties".into(), Value::Bool(false));
            Value::Object(variant)
        })
        .collect::<Vec<_>>();
    *schema = schemars::Schema::from(serde_json::Map::from_iter([(
        "oneOf".into(),
        Value::Array(variants),
    )]));
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEvent {
    pub run_id: String,
    pub workflow_version: u64,
    /// The source identity of the admitted artifact the run executes; the
    /// run overlay's node ids are that artifact's.
    pub definition: String,
    pub sequence: u64,
    pub node_id: String,
    pub status: RunStatus,
    #[serde(default)]
    pub display_delta: DisplayDelta,
    pub display: DisplayState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_key: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
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
        let code = error.code();
        let mut details = match &error {
            GraphRenderError::UnsupportedSchemaVersion(refusal) => json!({
                "found": refusal.found,
                "supportedRange": refusal.reads.supported(),
                "fleetWriterVersion": refusal.reads.recorded(),
            }),
            GraphRenderError::DuplicateNodeId { id } => json!({ "id": id }),
            GraphRenderError::UnknownNodeReference {
                edge_id, endpoint, ..
            } => json!({ "edgeId": edge_id, "endpoint": endpoint }),
            GraphRenderError::InvalidNodePayload { message, .. }
            | GraphRenderError::InvalidExpression { message, .. }
            | GraphRenderError::InvalidAssignmentTarget { message, .. }
            | GraphRenderError::InvalidOpaqueSource { message, .. } => {
                json!({ "reason": message })
            }
            GraphRenderError::DuplicateProcessName { name } => json!({ "name": name }),
            GraphRenderError::CanonicalSource(_) => json!({}),
            GraphRenderError::RenderedSourceInvalid { message } => {
                json!({ "reason": message })
            }
            // Future render failures retain their stable library code without
            // the example inventing an unowned detail schema.
            _ => json!({}),
        };
        if !matches!(&error, GraphRenderError::DuplicateNodeId { .. })
            && let Some(node_id) = error.node_id()
        {
            details["nodeId"] = json!(node_id);
        }
        if let Some(field) = error.field() {
            details["field"] = json!(field);
        }
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code, message, details)
    }

    pub(crate) fn projection(error: WorkflowGraphBuildError) -> Self {
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
        Self::render(GraphRenderError::InvalidExpression {
            node_id: node_id.to_string(),
            field: field.to_string(),
            message: message.into(),
        })
    }

    pub(crate) fn invalid_assignment_target(
        node_id: &str,
        field: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self::render(GraphRenderError::InvalidAssignmentTarget {
            node_id: node_id.to_string(),
            field,
            message: message.into(),
        })
    }

    pub(crate) fn invalid_node_payload(node_id: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        let host_message = format!("node `{node_id}` has an invalid payload: {message}");
        let mut response = Self::render(GraphRenderError::InvalidNodePayload {
            node_id: node_id.to_string(),
            message,
        });
        response.body.error.message = host_message;
        response
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
