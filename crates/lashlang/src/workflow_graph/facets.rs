use super::*;
use crate::linker::{LashlangHostEnvironment, analyze_workflow_program};

/// Version of the optional, derived workflow type-facet contract.
pub const WORKFLOW_TYPE_FACET_SCHEMA_VERSION: u32 = 1;

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

/// Project source with optional host-derived, non-authoritative type facets.
///
/// Link errors become node diagnostics and never prevent graph projection. If
/// no environment is supplied, the result is the ordinary facet-free graph.
pub fn workflow_graph_from_source_with_facets(
    src: &str,
    environment: Option<&LashlangHostEnvironment>,
) -> Result<WorkflowGraph, WorkflowGraphBuildError> {
    // The workflow graph remains a source-language projection. AST-only nodes
    // cannot enter this projector: both input paths parse source, and
    // canonicalization rejects every AST-only shape with
    // NonSourceableExpression — Function/Call/Map from FIG-1302's private AST
    // dialect, and Try/Throw from FIG-1303's exception layer. The projector's
    // generic expression arms are therefore intentionally not widened for
    // either; `renderer_declines_try_and_throw_at_every_nesting` pins the
    // refusal that keeps them out, so a future producer that made these nodes
    // sourceable would fail there rather than silently degrade a graph here.
    let parsed = parse(src)?;
    let canonical = canonical_program_source(&parsed)?;
    let canonical_program = parse(&canonical)?;
    let analysis =
        environment.map(|environment| analyze_workflow_program(&canonical_program, environment));
    Ok(GraphProjector::new(&canonical, &canonical_program, analysis.as_ref(), false).project())
}

pub(super) fn projected_node_type_facets(
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
                kind: diagnostic.kind().to_string(),
                message: diagnostic.to_string(),
                span: diagnostic.span(),
            })
            .collect(),
    })
}
