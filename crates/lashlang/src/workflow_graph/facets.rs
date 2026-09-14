use serde::{Deserialize, Serialize};

use super::{Span, WorkflowNodeId};
use crate::ast::{Expr, TypeExpr};
use crate::linker::WorkflowLinkAnalysis;

/// Version of the optional, derived workflow type-facet contract.
pub const WORKFLOW_TYPE_FACET_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowNodeTypeFacets {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_variables: Vec<WorkflowTypedVariable>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_arguments: Vec<WorkflowExpectedArgument>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<WorkflowTypeDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowTypedVariable {
    pub name: String,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowExpectedArgument {
    pub slot: String,
    pub ty: TypeExpr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowTypeDiagnostic {
    pub node_id: WorkflowNodeId,
    pub kind: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
}

/// Derive one node's optional, non-authoritative type facets from a link
/// analysis.
///
/// The facets describe types, never syntax, so the derivation stays in
/// `lashlang` while the projector that calls it lives in `lash-typescript`.
pub fn projected_node_type_facets(
    analysis: Option<&WorkflowLinkAnalysis>,
    expression: &Expr,
    available_variables: &[String],
    id: &WorkflowNodeId,
) -> Option<WorkflowNodeTypeFacets> {
    let facts = analysis?.facts_for(expression)?;
    Some(WorkflowNodeTypeFacets {
        available_variables: available_variables
            .iter()
            .map(|name| WorkflowTypedVariable {
                name: name.clone(),
                ty: facts
                    .available_variables
                    .get(name)
                    .cloned()
                    .unwrap_or(TypeExpr::Any),
            })
            .collect(),
        expected_arguments: facts
            .expected_arguments
            .iter()
            .map(|argument| WorkflowExpectedArgument {
                slot: argument.slot.clone(),
                ty: argument.ty.clone(),
            })
            .collect(),
        diagnostics: facts
            .diagnostics
            .iter()
            .map(|diagnostic| WorkflowTypeDiagnostic {
                node_id: id.clone(),
                kind: diagnostic.error.kind().to_string(),
                message: diagnostic.error.to_string(),
                span: diagnostic.span,
            })
            .collect(),
    })
}
