//! The workflow graph's typed document: value types, deterministic node
//! identity, and structured execution-site descriptors.
//!
//! The graph is deliberately a semantic, canonical view rather than a CST:
//! comments and authored formatting are discarded. Hosts own graph mutation,
//! drafts, layout, and versioning.
//!
//! Projection from IR lives here, beside the IR (ADR 0100 R8), and names no
//! source syntax: a dialect injects the text of opaque statements. Printing a
//! graph back to source and parsing edited node text belong to the dialect
//! (`lash_typescript::workflow_graph` for TypeScript).

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use lash_sansio::WorkflowExecutionSite;
use lash_sansio::core_support::Blake3DomainHasher;

use crate::ast::{
    AssignTarget, AstString, Expr, FunctionDecl, ProcessOrigin, ProcessParam, ProcessSignalDecl,
    TypeDecl, TypeExpr,
};
use crate::span::Span;

mod execution_sites;
mod facets;
mod ownership;
mod projection;

pub use execution_sites::execution_sites;
pub use facets::*;
pub use ownership::{
    ListedStatement, WorkflowBody, WorkflowBodySlot, WorkflowNodePath, WorkflowOwnership,
    WorkflowProjection, WorkflowStatement, statement_list,
};
pub use projection::{
    NoStatementText, WorkflowGraphProjector, WorkflowStatementText, else_if_chain,
    workflow_graph_from_artifact, workflow_graph_from_program,
};

/// Version of the serialized workflow graph contract. Version 15 closes the
/// execution-site kind vocabulary; v14 graph documents are refused.
pub const WORKFLOW_GRAPH_SCHEMA_VERSION: u32 = 15;

/// A deterministic node identifier minted from structural owner and AST path.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct WorkflowNodeId(String);

impl WorkflowNodeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Mints the structural identity shared by workflow projection and execution.
///
/// The owner and owner-relative AST path are the complete preimage. Artifact
/// identity belongs to the graph document and execution identity, not to a
/// node. A lifted process literal remains version-sensitive because its owner
/// includes the linker's body digest. The process declaration root uses the
/// empty path and has no runtime execution site.
pub fn workflow_node_id(owner: &str, path: &[u32]) -> WorkflowNodeId {
    let mut hasher = Blake3DomainHasher::new("lash-workflow-node/v3");
    hasher.update(owner.as_bytes());
    hasher.update([0]);
    for index in path {
        hasher.update(index.to_be_bytes());
    }
    WorkflowNodeId(format!("node:{}", &hasher.finalize_hex()[..24]))
}

impl std::fmt::Display for WorkflowNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The single serializable graph document used for editing and run overlays.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct WorkflowGraph {
    pub schema_version: u32,
    /// Content identity of the projected definition.
    ///
    /// The TypeScript projector hashes the canonical source bytes under
    /// `lash-workflow-source/v3`. The BLAKE3 preimage is the big-endian `u64`
    /// domain length, the domain bytes, then the canonical source bytes.
    /// Projection from an IR value uses those same source bytes when the IR can
    /// be printed and reparsed; otherwise the final preimage component is the
    /// JSON-serialized [`crate::Program`]. This value identifies definition
    /// content. [`WORKFLOW_GRAPH_SCHEMA_VERSION`] identifies this document's
    /// wire shape, `facet_schema_version` identifies optional derived facts,
    /// and `module_ref` identifies compiled artifact bytes.
    pub source_identity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facet_schema_version: Option<u32>,
    #[serde(default)]
    pub declarations: Vec<WorkflowDeclaration>,
    pub main: WorkflowSubgraph,
}

impl WorkflowGraph {
    /// Decodes a JSON graph after checking its version field in isolation.
    ///
    /// A version mismatch wins over errors in the rest of the document. This
    /// keeps an unknown field or enum variant from hiding the compatibility
    /// boundary that explains why the document cannot be read.
    pub fn decode_json(json: &str) -> Result<Self, WorkflowGraphDecodeError> {
        let value = serde_json::from_str(json).map_err(WorkflowGraphDecodeError::Document)?;
        Self::decode_json_value(value)
    }

    /// Decodes an already-parsed JSON graph with the same version-first fence
    /// as [`Self::decode_json`].
    pub fn decode_json_value(
        mut value: serde_json::Value,
    ) -> Result<Self, WorkflowGraphDecodeError> {
        let found = value
            .get("schema_version")
            .ok_or(WorkflowGraphDecodeError::MissingSchemaVersion)?
            .as_u64()
            .and_then(|version| u32::try_from(version).ok())
            .ok_or(WorkflowGraphDecodeError::InvalidSchemaVersion)?;
        if found != WORKFLOW_GRAPH_SCHEMA_VERSION {
            return Err(WorkflowGraphDecodeError::UnsupportedSchemaVersion {
                found,
                expected: WORKFLOW_GRAPH_SCHEMA_VERSION,
            });
        }
        let facets_are_current = value
            .get("facet_schema_version")
            .and_then(serde_json::Value::as_u64)
            == Some(u64::from(WORKFLOW_TYPE_FACET_SCHEMA_VERSION));
        if !facets_are_current {
            strip_type_facets(&mut value);
            if let Some(document) = value.as_object_mut() {
                document.remove("facet_schema_version");
            }
        }
        let encoded = serde_json::to_string(&value).map_err(WorkflowGraphDecodeError::Document)?;
        let mut deserializer = serde_json::Deserializer::from_str(&encoded);
        let mut unknown_fields = Vec::new();
        let wire: WorkflowGraphWire = serde_ignored::deserialize(&mut deserializer, |path| {
            unknown_fields.push(path.to_string());
        })
        .map_err(WorkflowGraphDecodeError::Document)?;
        if let Some(path) = unknown_fields.into_iter().next() {
            let field = path.rsplit('.').next().unwrap_or(path.as_str());
            return Err(WorkflowGraphDecodeError::Document(
                <serde_json::Error as serde::de::Error>::custom(format!(
                    "unknown field `{field}` at {path}"
                )),
            ));
        }
        Ok(Self {
            schema_version: wire.schema_version,
            source_identity: wire.source_identity,
            facet_schema_version: wire.facet_schema_version,
            declarations: wire.declarations,
            main: wire.main,
        })
    }

    pub fn process(&self, name: &str) -> Option<&WorkflowProcess> {
        self.declarations
            .iter()
            .find_map(|declaration| match declaration {
                WorkflowDeclaration::Process(process) if process.name.as_str() == name => {
                    Some(process)
                }
                _ => None,
            })
    }

    pub fn nodes(&self) -> impl Iterator<Item = &WorkflowNode> {
        let mut nodes = Vec::new();
        collect_subgraph_nodes(&self.main, &mut nodes);
        for declaration in &self.declarations {
            if let WorkflowDeclaration::Process(process) = declaration {
                collect_subgraph_nodes(&process.body, &mut nodes);
            }
        }
        nodes.into_iter()
    }
}

fn strip_type_facets(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                strip_type_facets(value);
            }
        }
        serde_json::Value::Object(object) => {
            if object.contains_key("nodes") && object.contains_key("edges") {
                if let Some(nodes) = object
                    .get_mut("nodes")
                    .and_then(serde_json::Value::as_array_mut)
                {
                    for node in nodes {
                        if let Some(node) = node.as_object_mut() {
                            node.remove("type_facets");
                        }
                        strip_type_facets(node);
                    }
                }
            } else {
                for value in object.values_mut() {
                    strip_type_facets(value);
                }
            }
        }
        _ => {}
    }
}

fn deserialize_strict<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    let mut unknown_field = None;
    let value = serde_ignored::deserialize(deserializer, |path| {
        if unknown_field.is_none() {
            unknown_field = Some(path.to_string());
        }
    })?;
    if let Some(path) = unknown_field {
        let field = path.rsplit('.').next().unwrap_or(path.as_str());
        return Err(serde::de::Error::custom(format!(
            "unknown field `{field}` at {path}"
        )));
    }
    Ok(value)
}

fn deserialize_tolerant<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}

impl<'de> Deserialize<'de> for WorkflowGraph {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        Self::decode_json_value(value).map_err(serde::de::Error::custom)
    }
}

/// Pairs a submitted graph with its own canonical reprojection.
///
/// This is a structural check, not semantic matching across definition
/// revisions. A node pairs only when exactly one node from each graph occupies
/// the same root, nested container-slot path, and index. Missing or duplicate
/// occupants are reported without choosing a candidate.
pub fn reconcile(
    submitted: &WorkflowGraph,
    reprojected: &WorkflowGraph,
) -> WorkflowGraphReconciliation {
    let submitted = nodes_by_structural_location(submitted);
    let reprojected = nodes_by_structural_location(reprojected);
    let locations = submitted
        .keys()
        .chain(reprojected.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut result = WorkflowGraphReconciliation::default();
    let mut candidates = Vec::new();

    for location in locations {
        let submitted_id = submitted.get(&location);
        let reprojected_id = reprojected.get(&location);
        match (submitted_id, reprojected_id) {
            (Some(submitted), Some(reprojected)) => candidates.push(WorkflowGraphReconcilePair {
                location,
                submitted: submitted.clone(),
                reprojected: reprojected.clone(),
            }),
            (None, Some(reprojected)) => {
                result.unmatched.push(WorkflowGraphUnmatchedNode {
                    location: location.clone(),
                    side: WorkflowGraphReconcileSide::Reprojected,
                    id: reprojected.clone(),
                });
            }
            (Some(submitted), None) => {
                result.unmatched.push(WorkflowGraphUnmatchedNode {
                    location: location.clone(),
                    side: WorkflowGraphReconcileSide::Submitted,
                    id: submitted.clone(),
                });
            }
            (None, None) => {}
        }
    }
    let mut ambiguous_pairs = BTreeSet::new();
    for (side, locations_by_id) in [
        (
            WorkflowGraphReconcileSide::Submitted,
            structural_locations_by_id(&submitted),
        ),
        (
            WorkflowGraphReconcileSide::Reprojected,
            structural_locations_by_id(&reprojected),
        ),
    ] {
        for (id, locations) in locations_by_id {
            if locations.len() <= 1 {
                continue;
            }
            let indexes = candidates
                .iter()
                .enumerate()
                .filter_map(|(index, pair)| {
                    let candidate_id = match side {
                        WorkflowGraphReconcileSide::Submitted => &pair.submitted,
                        WorkflowGraphReconcileSide::Reprojected => &pair.reprojected,
                    };
                    (candidate_id == &id).then_some(index)
                })
                .collect::<Vec<_>>();
            ambiguous_pairs.extend(indexes.iter().copied());
            result.ambiguous.push(WorkflowGraphAmbiguousNode {
                side,
                id,
                locations,
                candidates: indexes
                    .into_iter()
                    .map(|index| candidates[index].clone())
                    .collect(),
            });
        }
    }
    result.pairs.extend(
        candidates
            .into_iter()
            .enumerate()
            .filter_map(|(index, pair)| (!ambiguous_pairs.contains(&index)).then_some(pair)),
    );
    result
}

/// Result of checked workflow-graph reprojection pairing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowGraphReconciliation {
    pub pairs: Vec<WorkflowGraphReconcilePair>,
    pub unmatched: Vec<WorkflowGraphUnmatchedNode>,
    pub ambiguous: Vec<WorkflowGraphAmbiguousNode>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowGraphReconcilePair {
    pub location: WorkflowGraphStructuralLocation,
    pub submitted: WorkflowNodeId,
    pub reprojected: WorkflowNodeId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowGraphUnmatchedNode {
    pub location: WorkflowGraphStructuralLocation,
    pub side: WorkflowGraphReconcileSide,
    pub id: WorkflowNodeId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowGraphAmbiguousNode {
    pub side: WorkflowGraphReconcileSide,
    pub id: WorkflowNodeId,
    pub locations: Vec<WorkflowGraphStructuralLocation>,
    pub candidates: Vec<WorkflowGraphReconcilePair>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowGraphReconcileSide {
    Submitted,
    Reprojected,
}

/// One node's structural address inside a workflow document.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowGraphStructuralLocation {
    pub root: WorkflowGraphStructuralRoot,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slot_path: Vec<WorkflowGraphStructuralSlot>,
    pub index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowGraphStructuralRoot {
    Main,
    Process(usize),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowGraphStructuralSlot {
    pub parent_index: usize,
    pub slot: String,
}

fn nodes_by_structural_location(
    graph: &WorkflowGraph,
) -> BTreeMap<WorkflowGraphStructuralLocation, WorkflowNodeId> {
    let mut locations = BTreeMap::new();
    collect_structural_locations(
        &graph.main,
        &WorkflowGraphStructuralRoot::Main,
        &[],
        &mut locations,
    );
    let mut process_index = 0;
    for declaration in &graph.declarations {
        let WorkflowDeclaration::Process(process) = declaration else {
            continue;
        };
        let root = WorkflowGraphStructuralRoot::Process(process_index);
        process_index += 1;
        locations.insert(
            WorkflowGraphStructuralLocation {
                root: root.clone(),
                slot_path: Vec::new(),
                index: 0,
            },
            process.id.clone(),
        );
        collect_structural_locations(
            &process.body,
            &root,
            &[WorkflowGraphStructuralSlot {
                parent_index: 0,
                slot: "body".to_string(),
            }],
            &mut locations,
        );
    }
    locations
}

fn structural_locations_by_id(
    nodes: &BTreeMap<WorkflowGraphStructuralLocation, WorkflowNodeId>,
) -> BTreeMap<WorkflowNodeId, Vec<WorkflowGraphStructuralLocation>> {
    let mut locations_by_id = BTreeMap::new();
    for (location, id) in nodes {
        locations_by_id
            .entry(id.clone())
            .or_insert_with(Vec::new)
            .push(location.clone());
    }
    locations_by_id
}

fn collect_structural_locations(
    graph: &WorkflowSubgraph,
    root: &WorkflowGraphStructuralRoot,
    slot_path: &[WorkflowGraphStructuralSlot],
    locations: &mut BTreeMap<WorkflowGraphStructuralLocation, WorkflowNodeId>,
) {
    for (index, node) in graph.nodes.iter().enumerate() {
        locations.insert(
            WorkflowGraphStructuralLocation {
                root: root.clone(),
                slot_path: slot_path.to_vec(),
                index,
            },
            node.id.clone(),
        );
        if let WorkflowNodeKind::Container(container) = &node.kind {
            for (slot, child) in container.child_subgraphs() {
                let mut child_path = slot_path.to_vec();
                child_path.push(WorkflowGraphStructuralSlot {
                    parent_index: index,
                    slot: slot.to_string(),
                });
                collect_structural_locations(child, root, &child_path, locations);
            }
        }
    }
}

/// A refusal from the version-first [`WorkflowGraph`] JSON decoder.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WorkflowGraphDecodeError {
    #[error("workflow graph document is missing `schema_version`")]
    MissingSchemaVersion,
    #[error("workflow graph document has a non-u32 `schema_version`")]
    InvalidSchemaVersion,
    #[error("unsupported workflow graph schema version {found}; expected {expected}")]
    UnsupportedSchemaVersion { found: u32, expected: u32 },
    #[error("invalid workflow graph document: {0}")]
    Document(#[source] serde_json::Error),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowGraphWire {
    schema_version: u32,
    source_identity: String,
    #[serde(default)]
    facet_schema_version: Option<u32>,
    #[serde(default)]
    declarations: Vec<WorkflowDeclaration>,
    main: WorkflowSubgraph,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowDeclaration {
    Type(#[serde(deserialize_with = "deserialize_strict")] TypeDecl),
    Process(WorkflowProcess),
    /// A declared pure function, carried through the document unprojected.
    ///
    /// A process becomes a container with a child subgraph because its body is
    /// made of durable steps the editor has to show and reorder. A function's
    /// body contains no effects by construction, so it contributes no steps and
    /// no execution sites: projecting it into nodes would invent graph
    /// structure with nothing behind it. It travels verbatim instead, exactly
    /// as `Type` does, so a round trip through the document is lossless.
    Function(#[serde(deserialize_with = "deserialize_strict")] FunctionDecl),
}

/// A named process is a container with its own child subgraph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowProcess {
    pub id: WorkflowNodeId,
    pub name: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub name_source: WorkflowNodeNameSource,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_strict")]
    pub params: Vec<ProcessParam>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_strict")]
    pub signals: Vec<ProcessSignalDecl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_strict")]
    pub return_ty: Option<TypeExpr>,
    /// Whether the process was declared or lifted from an inline literal.
    #[serde(default, skip_serializing_if = "ProcessOrigin::is_declared")]
    #[serde(deserialize_with = "deserialize_strict")]
    pub origin: ProcessOrigin,
    pub body: WorkflowSubgraph,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeNameSource {
    Label,
    Derived,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSubgraph {
    #[serde(default)]
    pub nodes: Vec<WorkflowNode>,
    #[serde(default)]
    pub edges: Vec<WorkflowEdge>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowNode {
    pub id: WorkflowNodeId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub name_source: WorkflowNodeNameSource,
    pub kind: WorkflowNodeKind,
    /// Identifiers visible before this node executes, in stable lexical order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_variables: Vec<String>,
    /// Optional host-derived type information. It is never used to render source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_tolerant")]
    pub type_facets: Option<WorkflowNodeTypeFacets>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<VariableVersion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[serde(deserialize_with = "deserialize_strict")]
    pub execution_sites: Vec<WorkflowExecutionSite>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_strict")]
    pub source_span: Option<Span>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowNodeKind {
    Data {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        #[serde(deserialize_with = "deserialize_strict")]
        expression: Expr,
    },
    Call {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        #[serde(deserialize_with = "deserialize_strict")]
        receiver: Expr,
        operation: String,
        #[serde(default)]
        arguments: Vec<WorkflowArgument>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        result_steps: Vec<WorkflowResultStep>,
    },
    Effect {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        effect: WorkflowEffectKind,
        #[serde(default)]
        arguments: Vec<WorkflowArgument>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        result_steps: Vec<WorkflowResultStep>,
    },
    Computation {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        #[serde(deserialize_with = "deserialize_strict")]
        expression: Expr,
    },
    StateUpdate {
        #[serde(deserialize_with = "deserialize_strict")]
        target: AssignTarget,
        #[serde(deserialize_with = "deserialize_strict")]
        expression: Expr,
    },
    Terminal {
        terminal: WorkflowTerminalKind,
        #[serde(deserialize_with = "deserialize_strict")]
        expression: Expr,
    },
    Container(WorkflowContainer),
    Opaque {
        source: String,
    },
}

/// One call or effect argument in graph order.
///
/// Type facets address these values with a serialized [`WorkflowSlotPath`].
/// Its typed call, argument, field, and index segments cannot collide when a
/// field contains punctuation. Nodes with several nested receiver calls add a
/// call segment in depth-first IR walk order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowArgument {
    Positional {
        #[serde(deserialize_with = "deserialize_strict")]
        value: Expr,
    },
    Named {
        #[serde(deserialize_with = "deserialize_strict")]
        fields: Vec<(AstString, Expr)>,
    },
}

/// Ordered wrappers around a call or effect, from the operation outwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowResultStep {
    Await,
    UnwrapResult,
}

/// Decomposes one receiver call into the graph's structural IR fields.
pub fn workflow_call_from_ir(
    expression: &Expr,
) -> Option<(Expr, String, Vec<WorkflowArgument>, Vec<WorkflowResultStep>)> {
    let (call, result_steps) = peel_call_result_steps(expression);
    let Expr::ReceiverCall {
        receiver,
        operation,
        args,
    } = call
    else {
        return None;
    };
    Some((
        receiver.as_ref().clone(),
        operation.to_string(),
        args.iter().map(workflow_argument_from_ir).collect(),
        result_steps,
    ))
}

/// Reconstructs the authoritative IR represented by a call node.
pub fn workflow_call_to_ir(
    receiver: &Expr,
    operation: &str,
    arguments: &[WorkflowArgument],
    result_steps: &[WorkflowResultStep],
) -> Expr {
    apply_result_steps(
        Expr::ReceiverCall {
            receiver: Box::new(receiver.clone()),
            operation: operation.into(),
            args: arguments.iter().map(workflow_argument_to_ir).collect(),
        },
        result_steps,
    )
}

/// Decomposes one effect into its exact kind, arguments, and outer result steps.
pub fn workflow_effect_from_ir(
    expression: &Expr,
) -> Option<(
    WorkflowEffectKind,
    Vec<WorkflowArgument>,
    Vec<WorkflowResultStep>,
)> {
    let (effect, result_steps) = peel_result_steps(expression);
    let (kind, args) = match effect {
        Expr::Await(value) => (WorkflowEffectKind::AwaitJoin, vec![value.as_ref().clone()]),
        Expr::WaitSignal { name } => (
            WorkflowEffectKind::WaitSignal,
            vec![Expr::String(name.clone())],
        ),
        Expr::SleepFor(value) => (WorkflowEffectKind::SleepFor, vec![value.as_ref().clone()]),
        Expr::SleepUntil(value) => (WorkflowEffectKind::SleepUntil, vec![value.as_ref().clone()]),
        Expr::Print(value) => (WorkflowEffectKind::Print, vec![value.as_ref().clone()]),
        Expr::Yield(value) => (WorkflowEffectKind::Yield, vec![value.as_ref().clone()]),
        Expr::Break => (WorkflowEffectKind::Break, Vec::new()),
        Expr::Continue => (WorkflowEffectKind::Continue, Vec::new()),
        _ => return None,
    };
    Some((
        kind,
        args.iter().map(workflow_argument_from_ir).collect(),
        result_steps,
    ))
}

/// Reconstructs the authoritative IR represented by an effect node.
pub fn workflow_effect_to_ir(
    effect: WorkflowEffectKind,
    arguments: &[WorkflowArgument],
    result_steps: &[WorkflowResultStep],
) -> Option<Expr> {
    let values = arguments
        .iter()
        .map(workflow_argument_to_ir)
        .collect::<Vec<_>>();
    let expression = match (effect, values.as_slice()) {
        (WorkflowEffectKind::AwaitJoin, [value]) => Expr::Await(Box::new(value.clone())),
        (WorkflowEffectKind::WaitSignal, [Expr::String(name)]) => {
            Expr::WaitSignal { name: name.clone() }
        }
        (WorkflowEffectKind::SleepFor, [value]) => Expr::SleepFor(Box::new(value.clone())),
        (WorkflowEffectKind::SleepUntil, [value]) => Expr::SleepUntil(Box::new(value.clone())),
        (WorkflowEffectKind::Print, [value]) => Expr::Print(Box::new(value.clone())),
        (WorkflowEffectKind::Yield, [value]) => Expr::Yield(Box::new(value.clone())),
        (WorkflowEffectKind::Break, []) => Expr::Break,
        (WorkflowEffectKind::Continue, []) => Expr::Continue,
        _ => return None,
    };
    Some(apply_result_steps(expression, result_steps))
}

fn workflow_argument_from_ir(value: &Expr) -> WorkflowArgument {
    match value {
        Expr::Record(fields) => WorkflowArgument::Named {
            fields: fields.clone(),
        },
        value => WorkflowArgument::Positional {
            value: value.clone(),
        },
    }
}

fn workflow_argument_to_ir(argument: &WorkflowArgument) -> Expr {
    match argument {
        WorkflowArgument::Positional { value } => value.clone(),
        WorkflowArgument::Named { fields } => Expr::Record(fields.clone()),
    }
}

fn peel_result_steps(expression: &Expr) -> (&Expr, Vec<WorkflowResultStep>) {
    let mut expression = expression;
    let mut outer_steps = Vec::new();
    loop {
        match expression {
            Expr::ResultUnwrap(inner) => {
                outer_steps.push(WorkflowResultStep::UnwrapResult);
                expression = inner;
            }
            _ => {
                outer_steps.reverse();
                return (expression, outer_steps);
            }
        }
    }
}

fn peel_call_result_steps(expression: &Expr) -> (&Expr, Vec<WorkflowResultStep>) {
    let mut expression = expression;
    let mut outer_steps = Vec::new();
    loop {
        match expression {
            Expr::Await(inner) => {
                outer_steps.push(WorkflowResultStep::Await);
                expression = inner;
            }
            Expr::ResultUnwrap(inner) => {
                outer_steps.push(WorkflowResultStep::UnwrapResult);
                expression = inner;
            }
            _ => {
                outer_steps.reverse();
                return (expression, outer_steps);
            }
        }
    }
}

fn apply_result_steps(mut expression: Expr, result_steps: &[WorkflowResultStep]) -> Expr {
    for step in result_steps {
        expression = match step {
            WorkflowResultStep::Await => Expr::Await(Box::new(expression)),
            WorkflowResultStep::UnwrapResult => Expr::ResultUnwrap(Box::new(expression)),
        };
    }
    expression
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowEffectKind {
    AwaitJoin,
    WaitSignal,
    SleepFor,
    SleepUntil,
    Print,
    Yield,
    Break,
    Continue,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTerminalKind {
    Finish,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "container_kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowContainer {
    If {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        #[serde(deserialize_with = "deserialize_strict")]
        condition: Expr,
        /// Whether the source's then branch is a statement block rather than a value expression.
        then_is_block: bool,
        /// Whether the source's else branch is a block rather than a direct value or `else if`.
        else_is_block: bool,
        then_graph: Box<WorkflowSubgraph>,
        else_graph: Box<WorkflowSubgraph>,
    },
    /// An iteration: the IR's element binding, iterable and bind, exactly as
    /// the loop runs them. A dialect's printer reads its authored loop header
    /// back off these fields.
    For {
        binding: String,
        #[serde(deserialize_with = "deserialize_strict")]
        iterable: Expr,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        bind: Option<Expr>,
        body: Box<WorkflowSubgraph>,
    },
    While {
        #[serde(deserialize_with = "deserialize_strict")]
        condition: Expr,
        body: Box<WorkflowSubgraph>,
    },
    ListComprehension {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        #[serde(deserialize_with = "deserialize_strict")]
        clauses: Vec<WorkflowListComprehensionClause>,
        element: Box<WorkflowSubgraph>,
    },
}

impl WorkflowContainer {
    pub fn child_subgraphs(&self) -> impl Iterator<Item = (&'static str, &WorkflowSubgraph)> {
        let children = match self {
            Self::If {
                then_graph,
                else_graph,
                ..
            } => [
                Some(("then", then_graph.as_ref())),
                Some(("else", else_graph.as_ref())),
            ],
            Self::For { body, .. } | Self::While { body, .. } => {
                [Some(("body", body.as_ref())), None]
            }
            Self::ListComprehension { element, .. } => [Some(("element", element.as_ref())), None],
        };
        children.into_iter().flatten()
    }

    pub fn child_subgraphs_mut(
        &mut self,
    ) -> impl Iterator<Item = (&'static str, &mut WorkflowSubgraph)> {
        let children = match self {
            Self::If {
                then_graph,
                else_graph,
                ..
            } => [
                Some(("then", then_graph.as_mut())),
                Some(("else", else_graph.as_mut())),
            ],
            Self::For { body, .. } | Self::While { body, .. } => {
                [Some(("body", body.as_mut())), None]
            }
            Self::ListComprehension { element, .. } => [Some(("element", element.as_mut())), None],
        };
        children.into_iter().flatten()
    }
}

/// One editable list-comprehension clause.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowListComprehensionClause {
    For {
        binding: String,
        #[serde(deserialize_with = "deserialize_strict")]
        iterable: Expr,
    },
    If {
        #[serde(deserialize_with = "deserialize_strict")]
        condition: Expr,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VariableVersion {
    pub variable: String,
    pub version: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowEdge {
    pub id: String,
    pub from: WorkflowNodeId,
    pub to: WorkflowNodeId,
    pub kind: WorkflowEdgeKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowEdgeKind {
    DataDependency { variable: String, version: u32 },
    Sequence,
}

impl WorkflowNodeId {
    /// Wraps an already-minted node identifier.
    ///
    /// Graph documents a host builds carry ids it read off a projection, so
    /// the constructor is public; the value is opaque everywhere else.
    pub fn new(id: String) -> Self {
        Self(id)
    }
}

fn collect_subgraph_nodes<'a>(graph: &'a WorkflowSubgraph, nodes: &mut Vec<&'a WorkflowNode>) {
    for node in &graph.nodes {
        nodes.push(node);
        if let WorkflowNodeKind::Container(container) = &node.kind {
            for (_, child) in container.child_subgraphs() {
                collect_subgraph_nodes(child, nodes);
            }
        }
    }
}

/// Extends an AST path with one child index.
///
/// Node identity is path-keyed, so the projector and the execution-site walk
/// must agree on how a child path is spelled; this is that single spelling.
#[expect(
    clippy::expect_used,
    reason = "a child index into an in-memory AST whose children are enumerated far below u32::MAX fits, per the message"
)]
pub fn child_path(path: &[u32], child: impl TryInto<u32>) -> Vec<u32> {
    let mut result = path.to_vec();
    result.push(child.try_into().ok().expect("AST child index fits u32"));
    result
}
