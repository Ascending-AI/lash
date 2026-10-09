//! The workflow document: a total, typed view of the semantic IR.
//!
//! A [`WorkflowGraph`] holds every construct of the program it projects as
//! typed IR. Nothing in it is source text and nothing is opaque: a `try`, a
//! `throw` and a nested scope are regions with typed children like every
//! other statement, and an expression the graph does not decompose travels as
//! its [`Expr`]. The document is the program:
//! [`workflow_program_from_graph`] rebuilds exactly the IR
//! [`WorkflowGraphProjector`] read, with no dialect involved.
//!
//! # Authority
//!
//! A document has two kinds of field.
//!
//! *Authoritative* fields are the program: each body's [`WorkflowBodyShape`]
//! (its statements in order, how they are grouped and the value that closes
//! them), each node's `kind` payload (bindings, targets, expressions,
//! arguments, child bodies), a node's or a process's `label`, the
//! declarations with their parameters, types, origins and wrappers, and
//! `private_bindings`.
//!
//! *Derived* fields are read views a projection computes from those: node and
//! process ids, edges, `available_variables`, `outputs`, `type_facets`,
//! `execution_sites`, a node's `name`, and
//! `source_identity`. Reconstruction never reads them, so editing one changes
//! nothing; [`WorkflowGraph::rederive`] recomputes them all from the
//! authoritative fields. An edited document is a draft: it names no admitted
//! source identity until it is admitted again.
//!
//! # Addresses
//!
//! A node is named by its [`WorkflowNodeId`], unique within its document and
//! minted from the node's owner and structural path. An expression inside a
//! node is named by a [`WorkflowSlotPath`] of [`crate::ExprSlot`] segments
//! from the node's statement ([`workflow_node_statement`]): one typed step
//! per child role, for every expression role of every IR variant. A path
//! stops at the statements of a child body, which are nodes of their own.
//!
//! Source text is a lens over the document, never part of it
//! (`lash_typescript::workflow_graph` for TypeScript).

/// version_surface = "coexist"
/// version_guard(items(LASH_WORKFLOW_NODE_DOMAIN_VERSION, workflow_node_id))
const LASH_WORKFLOW_NODE_DOMAIN_VERSION: &str = "lash-workflow-node/v3";

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use lash_sansio::WorkflowExecutionSite;
use lash_sansio::core_support::Blake3DomainHasher;

use crate::ast::{
    AssignPathStep, AssignTarget, AstString, AttributeAssignParts, Expr, FunctionDecl,
    LabelMetadata, ProcessOrigin, ProcessParam, TypeExpr, UpdateOperator,
};

mod admission;
mod body;
mod draft;
mod execution_sites;
mod facets;
mod ownership;
mod projection;
mod reconstruction;
#[cfg(test)]
mod totality_tests;

pub use admission::{
    WorkflowAdmission, WorkflowAdmissionDiagnostic, WorkflowAdmissionDiagnosticKind,
    WorkflowAdmissionLocation, WorkflowAdmissionRefusal, admit_workflow_graph,
};
pub use body::{WorkflowBodyItem, WorkflowBodyShape};
pub use draft::{
    WorkflowBindingRef, WorkflowBodyLayout, WorkflowBodyLayoutItem, WorkflowBodyRef,
    WorkflowCorrespondence, WorkflowCorrespondenceEntry, WorkflowDraft, WorkflowDraftHandle,
    WorkflowDraftOpenError, WorkflowDraftRevision, WorkflowEdgeDrag, WorkflowEdit,
    WorkflowEditDiagnostic, WorkflowEditDiagnosticKind, WorkflowEditLocation, WorkflowEditRefusal,
    WorkflowEditTransaction, WorkflowExpressionRef, WorkflowNodeSource,
};
pub use execution_sites::execution_sites;
pub use facets::*;
pub use ownership::{
    ListedStatement, WorkflowBody, WorkflowBodySlot, WorkflowNodePath, WorkflowOwnership,
    WorkflowProjection, WorkflowStatement, statement_list,
};
pub use projection::{
    WorkflowGraphProjector, else_if_chain, workflow_graph_from_artifact,
    workflow_graph_from_program,
};
pub use reconstruction::{
    WorkflowGraphError, workflow_node_statement, workflow_program_from_graph,
};

/// The interpretation of the semantic IR a workflow document carries: the
/// variants of [`Expr`] and of the graph's regions, the meaning of each child
/// slot, scoping and evaluation order (see `ast_slots.rs`). A document stamps
/// the interpretation it was written under, and a reader that does not
/// implement it refuses the document when it opens it rather than reading
/// its contents under different rules.
///
/// version_guard(items(WORKFLOW_IR_VERSION, admit_ir_version))
/// version_surface = "coexist"
/// format_outside_manifest = "the semantic interpretation of a derived document: a reader refuses another interpretation and reprojects from the module, so no stored state is reopened under it"
pub const WORKFLOW_IR_VERSION: u32 = 1;

/// Version of the serialized workflow graph contract. Version 15 closes the
/// execution-site kind vocabulary; v14 graph documents are refused. Version 16
/// (FIG-3571) projects the carrier IR: compound state updates carry their
/// operator, node ids come from canonical carrier paths, and non-finite
/// numbers use the IR number encoding; v15 graph documents are refused.
/// Version 17 (FIG-3655) adds `js_name` to `FunctionExpr` for the function's
/// ECMAScript-inferred name; v16 graph documents are refused. Version 18
/// (FIG-3700) adds call receivers: `FunctionExpr.receiver`, `Expr::MethodCall`
/// and `Expr::ThisCall`; v17 graph documents are refused. Version 19
/// (FIG-3730) adds the bitwise and shift operators to `CoercingUnaryOp` and
/// `CoercingBinaryOp`; v18 graph documents are refused. Version 20
/// (FIG-3652) adds the `CoercingUnaryOp::ToString` operator the lowerer now
/// wraps template and concatenation operands in; v19 graph documents are
/// refused. Version 21 (FIG-4038) retires the surface dialect's IR: the
/// comprehension container kind and the declaration and expression spellings
/// TypeScript never produces are gone; v20 graph documents are refused.
///
/// version_guard(
///     shapes(cover(WorkflowGraph)),
///     roots(
///         path = "crates/lash-vm/src/ast.rs", Declaration, ProcessSignature, ProcessTypeKind,
///         ProcessTypeWire,
///     ),
///     items(
///         path = "crates/lash-vm/src/workflow_graph/projection.rs",
///         path = "crates/lash-vm/src/artifact.rs",
///         path = "crates/lash-typescript/src/workflow_graph/mod.rs",
///         workflow_graph_from_source_with_facets, workflow_graph_from_program,
///         workflow_graph_from_artifact, project_process, project_literal_process, project_node,
///         source_identity, edge,
///     ),
///     items(workflow_node_id),
///     items(
///         path = "crates/lash-vm/src/workflow_graph/execution_sites.rs",
///         path = "crates/lash-vm/src/workflow_graph/ownership.rs",
///         path = "crates/lash-vm/src/ast_roles.rs", path = "crates/lash-vm/src/tracking.rs",
///         from_indices, indices, path_for_ast, for_main, for_process, ownership_map,
///         into_ownership_map, workflow_projection, statement_list, push_statement_list,
///         is_statement_list, collect_body, collect_statement, statement_value, map_node_subtree,
///         check_shape, process_wrapper_run_path, execution_sites, collect_execution_sites,
///         push_execution_site_descriptor, collect_child_execution_sites, workflow_owner,
///         node_site, branch_site,
///     ),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_manifest = "WorkflowGraphSchema"
pub const WORKFLOW_GRAPH_SCHEMA_VERSION: u32 = 21;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the document with version
/// 21's shape. A derived projection registers no lift: while `F` is N's epoch
/// its readers admit the version `F` pins its writers to, and after finalize
/// an older document is regenerated from its module (FIG-4262).
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_manifest = "WorkflowGraphSchema"
pub const WORKFLOW_GRAPH_SCHEMA_VERSION: u32 = 22;

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
    let mut hasher = Blake3DomainHasher::new(LASH_WORKFLOW_NODE_DOMAIN_VERSION);
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
    /// The interpretation of the semantic IR this document's regions and
    /// expressions are written under ([`WORKFLOW_IR_VERSION`]).
    pub ir_version: u32,
    /// The definition identity of the admitted module artifact this graph
    /// projects ([`crate::ModuleArtifact::source_identity`]), which the
    /// module's traces carry too. A draft projected from source that has not
    /// been admitted claims no runtime identity and carries `None`.
    /// [`WORKFLOW_GRAPH_SCHEMA_VERSION`] identifies this document's wire shape,
    /// `facet_schema_version` identifies optional derived facts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facet_schema_version: Option<u32>,
    /// The main-level bindings that are the front end's own slots rather
    /// than session-visible names ([`crate::Program::private_bindings`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub private_bindings: BTreeSet<AstString>,
    #[serde(default)]
    pub declarations: Vec<WorkflowDeclaration>,
    pub main: WorkflowSubgraph,
}

impl WorkflowGraph {
    /// Refuses a document written under an IR interpretation this build does
    /// not implement.
    pub fn admit_ir_version(found: u32) -> Result<(), WorkflowIrVersionRefusal> {
        if found == WORKFLOW_IR_VERSION {
            Ok(())
        } else {
            Err(WorkflowIrVersionRefusal {
                found,
                supported: WORKFLOW_IR_VERSION,
            })
        }
    }

    /// The canonical draft of this document: its authoritative content
    /// reconstructed to IR and projected again, so every derived view (ids,
    /// edges, available variables, outputs, execution sites) is recomputed
    /// from that content alone. The draft names no source identity and
    /// carries no spans or facets; admission supplies those.
    pub fn rederive(&self) -> Result<Self, WorkflowGraphError> {
        let program = workflow_program_from_graph(self)?;
        Ok(workflow_graph_from_program(&program))
    }

    /// Admit a document stamp against the decoder-backed read window under
    /// `fleet_format`. Derived graphs have no lift: the fleet pin is readable
    /// during a roll, and after finalize an older graph must be regenerated.
    pub fn admit_schema_version_for_fleet(
        found: u32,
        fleet_format: lash_core_execution::FleetFormat,
    ) -> Result<(), WorkflowGraphVersionRefusal> {
        let reads = fleet_format.read_window(lash_core_execution::surface_format!(
            WORKFLOW_GRAPH_SCHEMA_VERSION
        ));
        if reads.admits(found) {
            Ok(())
        } else {
            Err(WorkflowGraphVersionRefusal { found, reads })
        }
    }

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
    pub fn decode_json_value(value: serde_json::Value) -> Result<Self, WorkflowGraphDecodeError> {
        Self::decode_json_value_for_fleet(value, lash_core_execution::FleetFormat::current())
    }

    /// The fleet leg of [`Self::decode_json_value`]: the version fence admits
    /// the graph surface's read window under `fleet_format` — this build's
    /// newest, and the version `F` pins the projection's writers to. A
    /// derived projection lifts nothing, so an older document is refused and
    /// regenerated from its module.
    pub fn decode_json_value_for_fleet(
        mut value: serde_json::Value,
        fleet_format: lash_core_execution::FleetFormat,
    ) -> Result<Self, WorkflowGraphDecodeError> {
        let found = value
            .get("schema_version")
            .ok_or(WorkflowGraphDecodeError::MissingSchemaVersion)?
            .as_u64()
            .and_then(|version| u32::try_from(version).ok())
            .ok_or(WorkflowGraphDecodeError::InvalidSchemaVersion)?;
        Self::admit_schema_version_for_fleet(found, fleet_format)
            .map_err(WorkflowGraphDecodeError::UnsupportedSchemaVersion)?;
        let ir_version = value
            .get("ir_version")
            .ok_or(WorkflowGraphDecodeError::MissingIrVersion)?
            .as_u64()
            .and_then(|version| u32::try_from(version).ok())
            .ok_or(WorkflowGraphDecodeError::InvalidIrVersion)?;
        Self::admit_ir_version(ir_version)?;
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
            ir_version: wire.ir_version,
            source_identity: wire.source_identity,
            facet_schema_version: wire.facet_schema_version,
            private_bindings: wire.private_bindings,
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

/// Removes every node's `type_facets` from an undecoded document, walking
/// exactly the places a document holds nodes.
fn strip_type_facets(document: &mut serde_json::Value) {
    fn subgraph(graph: Option<&mut serde_json::Value>) {
        let Some(body) = graph.and_then(|graph| graph.get_mut("body")) else {
            return;
        };
        node(body.get_mut("node"));
        items(body.get_mut("items"));
    }
    fn items(list: Option<&mut serde_json::Value>) {
        let Some(list) = list.and_then(serde_json::Value::as_array_mut) else {
            return;
        };
        for item in list {
            node(item.get_mut("node"));
            items(
                item.get_mut("group")
                    .and_then(|group| group.get_mut("items")),
            );
        }
    }
    fn node(node: Option<&mut serde_json::Value>) {
        let Some(node) = node.and_then(serde_json::Value::as_object_mut) else {
            return;
        };
        node.remove("type_facets");
        let Some(kind) = node.get_mut("kind") else {
            return;
        };
        for child in ["then_graph", "else_graph", "body", "finally"] {
            subgraph(kind.get_mut(child));
        }
        subgraph(
            kind.get_mut("catch")
                .and_then(|catch| catch.get_mut("body")),
        );
    }
    subgraph(document.get_mut("main"));
    let declarations = document
        .get_mut("declarations")
        .and_then(serde_json::Value::as_array_mut);
    for declaration in declarations.into_iter().flatten() {
        subgraph(declaration.get_mut("body"));
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

/// The decoder's supported range and the fleet-selected writer pin that
/// refused a workflow graph. The pin is separate because it need not be
/// contiguous with the supported range.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error(
    "unsupported workflow graph schema version {found}; supported range {supported}, fleet writer version {recorded}; regenerate from the module",
    supported = .reads.supported(),
    recorded = .reads.recorded(),
)]
pub struct WorkflowGraphVersionRefusal {
    pub found: u32,
    pub reads: lash_core_execution::store::ReadWindow,
}

/// The IR interpretation that refused a workflow document.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error(
    "unsupported workflow IR interpretation {found}; this build implements {supported}; reproject from the module"
)]
pub struct WorkflowIrVersionRefusal {
    pub found: u32,
    pub supported: u32,
}

/// A refusal from the version-first [`WorkflowGraph`] JSON decoder.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WorkflowGraphDecodeError {
    #[error("workflow graph document is missing `schema_version`")]
    MissingSchemaVersion,
    #[error("workflow graph document has a non-u32 `schema_version`")]
    InvalidSchemaVersion,
    #[error(transparent)]
    UnsupportedSchemaVersion(#[from] WorkflowGraphVersionRefusal),
    #[error("workflow graph document is missing `ir_version`")]
    MissingIrVersion,
    #[error("workflow graph document has a non-u32 `ir_version`")]
    InvalidIrVersion,
    #[error(transparent)]
    UnsupportedIrVersion(#[from] WorkflowIrVersionRefusal),
    #[error("invalid workflow graph document: {0}")]
    Document(#[source] serde_json::Error),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowGraphWire {
    schema_version: u32,
    ir_version: u32,
    #[serde(default)]
    source_identity: Option<String>,
    #[serde(default)]
    facet_schema_version: Option<u32>,
    #[serde(default)]
    private_bindings: BTreeSet<AstString>,
    #[serde(default)]
    declarations: Vec<WorkflowDeclaration>,
    main: WorkflowSubgraph,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowDeclaration {
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

impl WorkflowProcess {
    /// The name a person reads the process by: its label's title, or its
    /// name when it has no label.
    pub fn display_name(&self) -> &str {
        self.label
            .as_ref()
            .map_or(self.name.as_str(), |label| label.title.as_str())
    }
}

/// A named process is a container with its own child subgraph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowProcess {
    pub id: WorkflowNodeId,
    pub name: String,
    /// The authored label, when the process has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_strict")]
    pub label: Option<LabelMetadata>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_strict")]
    pub params: Vec<ProcessParam>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_strict")]
    pub return_ty: Option<TypeExpr>,
    /// Whether the process was declared or lifted from an inline literal.
    #[serde(default, skip_serializing_if = "ProcessOrigin::is_declared")]
    #[serde(deserialize_with = "deserialize_strict")]
    pub origin: ProcessOrigin,
    /// The failure wrapper around the authored run body, when the process
    /// body has one ([`crate::StructuralRole::ProcessWrapper`]). `body` is
    /// then the run function's body; without a wrapper it is the whole
    /// process body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrapper: Option<Box<WorkflowProcessWrapper>>,
    pub body: WorkflowSubgraph,
}

/// The process failure wrapper, without the run body it wraps: the run
/// function finishes the process with its value, and the catch fails the
/// process with what the body throws.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowProcessWrapper {
    /// The run function's own name, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<AstString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub js_name: Option<AstString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<AstString>,
    /// The run function's parameters, bound to `arguments` in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<AstString>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captures: Vec<AstString>,
    /// The builtin the run call goes through, when it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<WorkflowRunDriver>,
    /// The arguments the run call passes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[serde(deserialize_with = "deserialize_strict")]
    pub arguments: Vec<Expr>,
    /// The binding the wrapper's catch fails the process with.
    pub catch_binding: AstString,
}

/// A builtin that drives a process's run function: it receives the function
/// first, then `arguments`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRunDriver {
    pub builtin: AstString,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[serde(deserialize_with = "deserialize_strict")]
    pub arguments: Vec<Expr>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSubgraph {
    /// The body's statements, in execution order, as the IR holds them.
    #[serde(default)]
    pub body: WorkflowBodyShape,
    /// Derived: sequence follows statement order and data dependencies
    /// follow the bindings the nodes' expressions read.
    #[serde(default)]
    pub edges: Vec<WorkflowEdge>,
}

impl WorkflowSubgraph {
    /// The body's statements in execution order, through every group and not
    /// into any child body.
    pub fn nodes(&self) -> Vec<&WorkflowNode> {
        self.body.nodes()
    }

    /// [`Self::nodes`], for changing them in place.
    pub fn nodes_mut(&mut self) -> Vec<&mut WorkflowNode> {
        self.body.nodes_mut()
    }

    /// Whether the body is a statement list rather than one bare statement.
    pub fn is_statement_list(&self) -> bool {
        matches!(self.body, WorkflowBodyShape::List { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowNode {
    pub id: WorkflowNodeId,
    /// Derived: what the statement is, read off its IR.
    pub name: String,
    /// The authored label, when the statement has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_strict")]
    pub label: Option<LabelMetadata>,
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
}

impl WorkflowNode {
    /// The name a person reads the node by: its label's title, or its
    /// derived name when it has no label.
    pub fn display_name(&self) -> &str {
        self.label
            .as_ref()
            .map_or(self.name.as_str(), |label| label.title.as_str())
    }
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
    Effect(WorkflowEffect),
    Computation {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        #[serde(deserialize_with = "deserialize_strict")]
        expression: Expr,
    },
    StateUpdate(WorkflowStateWrite),
    Terminal(WorkflowTerminal),
    /// Throws `value`: control transfers to the nearest enclosing catch, or
    /// fails the process when there is none.
    Throw {
        #[serde(deserialize_with = "deserialize_strict")]
        value: Expr,
    },
    Container(WorkflowContainer),
}

/// An effect a statement performs, with exactly the operands its kind takes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "effect", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowEffect {
    /// Waits for `value` to settle, then applies `result_steps` to what it
    /// settled with.
    AwaitJoin {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        #[serde(deserialize_with = "deserialize_strict")]
        value: Expr,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        result_steps: Vec<WorkflowResultStep>,
    },
    SleepFor {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        #[serde(deserialize_with = "deserialize_strict")]
        duration: Expr,
    },
    Print {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        #[serde(deserialize_with = "deserialize_strict")]
        value: Expr,
    },
    /// Leaves the nearest enclosing loop. It has no value to bind.
    Break,
    /// Starts the next iteration of the nearest enclosing loop. It has no
    /// value to bind.
    Continue,
}

impl WorkflowEffect {
    /// Reads `expression` as an effect, with `binding` the target its value
    /// is assigned to. `None` when the expression is no effect, or is one
    /// this kind of effect cannot spell: a bound `break`, or an unwrapped
    /// `sleep`.
    pub fn from_ir(binding: Option<&AssignTarget>, expression: &Expr) -> Option<Self> {
        let binding = binding.cloned();
        match expression {
            Expr::SleepFor(duration) => Some(Self::SleepFor {
                binding,
                duration: duration.as_ref().clone(),
            }),
            Expr::Print(value) => Some(Self::Print {
                binding,
                value: value.as_ref().clone(),
            }),
            Expr::Break if binding.is_none() => Some(Self::Break),
            Expr::Continue if binding.is_none() => Some(Self::Continue),
            _ => {
                let (effect, result_steps) = peel_result_steps(expression);
                let Expr::Await(value) = effect else {
                    return None;
                };
                Some(Self::AwaitJoin {
                    binding,
                    value: value.as_ref().clone(),
                    result_steps,
                })
            }
        }
    }

    /// The statement the effect spells.
    pub fn to_ir(&self) -> Expr {
        let (binding, expression) = match self {
            Self::AwaitJoin {
                binding,
                value,
                result_steps,
            } => (
                binding,
                apply_result_steps(Expr::Await(Box::new(value.clone())), result_steps),
            ),
            Self::SleepFor { binding, duration } => {
                (binding, Expr::SleepFor(Box::new(duration.clone())))
            }
            Self::Print { binding, value } => (binding, Expr::Print(Box::new(value.clone()))),
            Self::Break => return Expr::Break,
            Self::Continue => return Expr::Continue,
        };
        match binding {
            Some(target) => Expr::Assign {
                target: target.clone(),
                expr: Box::new(expression),
            },
            None => expression,
        }
    }

    pub fn kind(&self) -> WorkflowEffectKind {
        match self {
            Self::AwaitJoin { .. } => WorkflowEffectKind::AwaitJoin,
            Self::SleepFor { .. } => WorkflowEffectKind::SleepFor,
            Self::Print { .. } => WorkflowEffectKind::Print,
            Self::Break => WorkflowEffectKind::Break,
            Self::Continue => WorkflowEffectKind::Continue,
        }
    }

    /// The target the effect's value is assigned to, if it has one.
    pub fn binding(&self) -> Option<&AssignTarget> {
        match self {
            Self::AwaitJoin { binding, .. }
            | Self::SleepFor { binding, .. }
            | Self::Print { binding, .. } => binding.as_ref(),
            Self::Break | Self::Continue => None,
        }
    }

    /// The binding field of an effect that can bind its value.
    pub fn binding_mut(&mut self) -> Option<&mut Option<AssignTarget>> {
        match self {
            Self::AwaitJoin { binding, .. }
            | Self::SleepFor { binding, .. }
            | Self::Print { binding, .. } => Some(binding),
            Self::Break | Self::Continue => None,
        }
    }
}

/// What a state update writes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "write", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowStateWrite {
    /// Assigns `value` to `target`.
    Plain {
        #[serde(deserialize_with = "deserialize_strict")]
        target: AssignTarget,
        #[serde(deserialize_with = "deserialize_strict")]
        value: Expr,
    },
    /// A member assignment that pins its reference base before it evaluates
    /// the value ([`crate::StructuralRole::AttributeAssign`]): writes the
    /// member `step` of the variable `root`.
    Member {
        root: AstString,
        step: WorkflowMemberStep,
        /// The slot the reference base is pinned in.
        base_slot: AstString,
        /// The slot the assigned value is held in.
        result_slot: AstString,
        /// The operator a compound update applies to the member's current
        /// value (`root.step op= operand`); a plain write has none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        update: Option<UpdateOperator>,
        /// The assigned value, or with `update` its right operand.
        #[serde(deserialize_with = "deserialize_strict")]
        operand: Expr,
    },
}

/// The member a pinned assignment writes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowMemberStep {
    Field {
        field: AstString,
    },
    /// A computed index, pinned in `key_slot` before the value is evaluated.
    Index {
        #[serde(deserialize_with = "deserialize_strict")]
        index: Expr,
        key_slot: AstString,
    },
}

impl WorkflowStateWrite {
    /// Reads a member assignment role whose object is a plain variable: the
    /// only member write a state update names.
    pub fn member_from_ir(expression: &Expr) -> Option<Self> {
        let Expr::Role {
            role: crate::StructuralRole::AttributeAssign,
            expr,
        } = expression
        else {
            return None;
        };
        let parts = AttributeAssignParts::of(expr)?;
        let Expr::Variable(root) = parts.object else {
            return None;
        };
        let step = match (parts.step, parts.key) {
            (crate::AttributeStep::Field(field), None) => WorkflowMemberStep::Field {
                field: field.clone(),
            },
            (crate::AttributeStep::Index(index), Some(key)) => WorkflowMemberStep::Index {
                index: index.clone(),
                key_slot: key.clone(),
            },
            _ => return None,
        };
        let (update, operand) = match parts.update {
            Some(update) => (Some(update.operator), update.operand),
            None => (None, parts.value),
        };
        Some(Self::Member {
            root: root.clone(),
            step,
            base_slot: parts.base.clone(),
            result_slot: parts.result.clone(),
            update,
            operand: operand.clone(),
        })
    }

    /// The statement the write spells.
    pub fn to_ir(&self) -> Expr {
        match self {
            Self::Plain { target, value } => Expr::Assign {
                target: target.clone(),
                expr: Box::new(value.clone()),
            },
            Self::Member {
                root,
                step,
                base_slot,
                result_slot,
                update,
                operand,
            } => {
                let step = match step {
                    WorkflowMemberStep::Field { field } => {
                        crate::AttributeWrite::Field(field.clone())
                    }
                    WorkflowMemberStep::Index { index, key_slot } => crate::AttributeWrite::Index {
                        key: key_slot.clone(),
                        index: index.clone(),
                    },
                };
                let value = match update {
                    None => operand.clone(),
                    Some(operator) => AttributeAssignParts::update_value(
                        base_slot,
                        &step,
                        *operator,
                        operand.clone(),
                    ),
                };
                AttributeAssignParts::build(
                    base_slot.clone(),
                    step,
                    result_slot.clone(),
                    Expr::Variable(root.clone()),
                    value,
                )
            }
        }
    }

    /// The variable the write updates.
    pub fn root(&self) -> &AstString {
        match self {
            Self::Plain { target, .. } => &target.root,
            Self::Member { root, .. } => root,
        }
    }

    /// The place the write assigns: for a member write, the one member step
    /// of its root.
    pub fn target(&self) -> AssignTarget {
        match self {
            Self::Plain { target, .. } => target.clone(),
            Self::Member { root, step, .. } => AssignTarget {
                root: root.clone(),
                steps: vec![match step {
                    WorkflowMemberStep::Field { field } => AssignPathStep::Field(field.clone()),
                    WorkflowMemberStep::Index { index, .. } => AssignPathStep::Index(index.clone()),
                }],
            },
        }
    }
}

/// How a statement ends its process or function.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "terminal", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowTerminal {
    /// Finishes the process with `value`.
    Finish {
        #[serde(deserialize_with = "deserialize_strict")]
        value: Expr,
    },
    /// Returns `value` from the enclosing function body, which in a process
    /// body is the process's finish.
    Return {
        #[serde(deserialize_with = "deserialize_strict")]
        value: Expr,
    },
    /// Fails the process with `value`.
    Fail {
        #[serde(deserialize_with = "deserialize_strict")]
        value: Expr,
    },
}

impl WorkflowTerminal {
    /// Reads `expression` as a terminal, or `None` when it is not one.
    pub fn from_ir(expression: &Expr) -> Option<Self> {
        Some(match expression {
            Expr::Finish(value) => Self::Finish {
                value: value.as_ref().clone(),
            },
            Expr::FunctionReturn(value) => Self::Return {
                value: value.as_ref().clone(),
            },
            Expr::Fail(value) => Self::Fail {
                value: value.as_ref().clone(),
            },
            _ => return None,
        })
    }

    /// The statement the terminal spells.
    pub fn to_ir(&self) -> Expr {
        match self {
            Self::Finish { value } => Expr::Finish(Box::new(value.clone())),
            Self::Return { value } => Expr::FunctionReturn(Box::new(value.clone())),
            Self::Fail { value } => Expr::Fail(Box::new(value.clone())),
        }
    }

    /// Whether the terminal finishes or fails.
    pub fn kind(&self) -> WorkflowTerminalKind {
        match self {
            Self::Finish { .. } | Self::Return { .. } => WorkflowTerminalKind::Finish,
            Self::Fail { .. } => WorkflowTerminalKind::Fail,
        }
    }

    pub fn value(&self) -> &Expr {
        match self {
            Self::Finish { value } | Self::Return { value } | Self::Fail { value } => value,
        }
    }
}

/// One call argument in graph order.
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

/// Ordered wrappers around a call or an await, from the operation outwards.
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

/// Which effect a [`WorkflowEffect`] is, without its operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowEffectKind {
    AwaitJoin,
    SleepFor,
    Print,
    Break,
    Continue,
}

/// Whether a [`WorkflowTerminal`] finishes or fails, without its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTerminalKind {
    Finish,
    Fail,
}

/// A statement that owns ordered child bodies.
///
/// `binding` is the target the container's value is assigned to, when the
/// statement is an assignment of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "container_kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowContainer {
    /// Evaluates `condition`, then runs exactly one branch.
    If {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        #[serde(deserialize_with = "deserialize_strict")]
        condition: Expr,
        then_graph: Box<WorkflowSubgraph>,
        else_graph: Box<WorkflowSubgraph>,
    },
    /// An iteration, exactly as the loop runs it: for each element of
    /// `iterable`, bind it to `element`, run `bind`, then run `body`.
    For {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        /// The binding each element is assigned to.
        element: String,
        /// The element binding's authored name, outside execution identity.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        authored_element: Option<String>,
        #[serde(deserialize_with = "deserialize_strict")]
        iterable: Expr,
        /// The generated statements that bind the element into the names the
        /// body reads, when the front end needs any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        bind: Option<Expr>,
        body: Box<WorkflowSubgraph>,
    },
    While {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        #[serde(deserialize_with = "deserialize_strict")]
        condition: Expr,
        body: Box<WorkflowSubgraph>,
    },
    /// A structured exception scope: `body` runs; a throw inside it runs
    /// `catch` with the thrown value bound; `finally` runs on every exit
    /// from either.
    Try {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        body: Box<WorkflowSubgraph>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        catch: Option<WorkflowCatch>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finally: Option<Box<WorkflowSubgraph>>,
    },
    /// An authored nested statement scope.
    Scope {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        binding: Option<AssignTarget>,
        body: Box<WorkflowSubgraph>,
    },
}

/// The catch clause of a [`WorkflowContainer::Try`]: `binding` names the
/// thrown value inside `body`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCatch {
    pub binding: String,
    pub body: Box<WorkflowSubgraph>,
}

impl WorkflowContainer {
    /// The target the container's value is assigned to, if any.
    pub fn binding(&self) -> Option<&AssignTarget> {
        match self {
            Self::If { binding, .. }
            | Self::For { binding, .. }
            | Self::While { binding, .. }
            | Self::Try { binding, .. }
            | Self::Scope { binding, .. } => binding.as_ref(),
        }
    }

    /// The name a loop's body reads its element by: the one name `bind`
    /// copies the element into when that is all it does, else the element
    /// binding itself. A derived view for display; `None` for other
    /// containers.
    pub fn loop_element_name(&self) -> Option<&str> {
        let Self::For { element, bind, .. } = self else {
            return None;
        };
        Some(projection::copied_binding(element, bind.as_ref()).unwrap_or(element.as_str()))
    }

    pub fn child_subgraphs(&self) -> impl Iterator<Item = (&'static str, &WorkflowSubgraph)> {
        let children = match self {
            Self::If {
                then_graph,
                else_graph,
                ..
            } => [
                Some(("then", then_graph.as_ref())),
                Some(("else", else_graph.as_ref())),
                None,
            ],
            Self::For { body, .. } | Self::While { body, .. } | Self::Scope { body, .. } => {
                [Some(("body", body.as_ref())), None, None]
            }
            Self::Try {
                body,
                catch,
                finally,
                ..
            } => [
                Some(("body", body.as_ref())),
                catch.as_ref().map(|catch| ("catch", catch.body.as_ref())),
                finally.as_deref().map(|finally| ("finally", finally)),
            ],
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
                None,
            ],
            Self::For { body, .. } | Self::While { body, .. } | Self::Scope { body, .. } => {
                [Some(("body", body.as_mut())), None, None]
            }
            Self::Try {
                body,
                catch,
                finally,
                ..
            } => [
                Some(("body", body.as_mut())),
                catch.as_mut().map(|catch| ("catch", catch.body.as_mut())),
                finally.as_deref_mut().map(|finally| ("finally", finally)),
            ],
        };
        children.into_iter().flatten()
    }
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
    for node in graph.nodes() {
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
