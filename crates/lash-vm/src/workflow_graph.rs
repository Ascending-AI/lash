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
//! *Authoritative* fields are the program: the ordered `nodes` of each body
//! and its [`WorkflowBodyForm`], each node's `kind` payload (bindings,
//! targets, expressions, arguments, child bodies), a labelled node's `name`
//! and `description` with `name_source`, the declarations with their
//! parameters, types, origins and wrappers, and `private_bindings`.
//!
//! *Derived* fields are read views a projection computes from those: node and
//! process ids, edges, `available_variables`, `outputs`, `type_facets`,
//! `execution_sites`, `source_span`, a derived node's `name`, and
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
    AssignTarget, AstString, Expr, FunctionDecl, ProcessOrigin, ProcessParam, TypeExpr,
};
use crate::span::Span;

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
pub use body::{WorkflowBodyForm, WorkflowCompletionGroup};
pub use draft::{
    WorkflowBindingRef, WorkflowBodyRef, WorkflowCorrespondence, WorkflowCorrespondenceEntry,
    WorkflowDraft, WorkflowDraftHandle, WorkflowDraftOpenError, WorkflowDraftRevision,
    WorkflowEdgeDrag, WorkflowEdit, WorkflowEditDiagnostic, WorkflowEditDiagnosticKind,
    WorkflowEditLocation, WorkflowEditRefusal, WorkflowEditTransaction, WorkflowNodeSource,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeNameSource {
    Label,
    Derived,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSubgraph {
    /// How the IR spells this ordered body.
    #[serde(default)]
    pub form: WorkflowBodyForm,
    /// The body's statements, in execution order.
    #[serde(default)]
    pub nodes: Vec<WorkflowNode>,
    /// Derived: sequence follows `nodes` order and data dependencies follow
    /// the bindings the nodes' expressions read.
    #[serde(default)]
    pub edges: Vec<WorkflowEdge>,
}

impl WorkflowSubgraph {
    /// Whether the body is a statement list rather than one bare statement.
    pub fn is_statement_list(&self) -> bool {
        !matches!(self.form, WorkflowBodyForm::Statement)
    }
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
        /// The assigned value, or with `update`, the operand the update applies
        /// to the target's current value (`target op= expression`).
        #[serde(deserialize_with = "deserialize_strict")]
        expression: Expr,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        update: Option<crate::UpdateOperator>,
        /// Set when the update is a member assignment that pins its
        /// reference base before evaluating the value
        /// ([`crate::StructuralRole::AttributeAssign`]): the slots it pins
        /// them in. `target` is then one member step of a variable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pinned: Option<WorkflowPinnedSlots>,
    },
    Terminal {
        terminal: WorkflowTerminalKind,
        #[serde(deserialize_with = "deserialize_strict")]
        expression: Expr,
    },
    /// Throws `value`: control transfers to the nearest enclosing catch, or
    /// fails the process when there is none.
    Throw {
        #[serde(deserialize_with = "deserialize_strict")]
        value: Expr,
    },
    Container(WorkflowContainer),
}

/// The slots a pinned member assignment evaluates through, in evaluation
/// order: the reference base, a computed index, then the assigned value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPinnedSlots {
    pub base: AstString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<AstString>,
    pub result: AstString,
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
        Expr::SleepFor(value) => (WorkflowEffectKind::SleepFor, vec![value.as_ref().clone()]),
        Expr::Print(value) => (WorkflowEffectKind::Print, vec![value.as_ref().clone()]),
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
        (WorkflowEffectKind::SleepFor, [value]) => Expr::SleepFor(Box::new(value.clone())),
        (WorkflowEffectKind::Print, [value]) => Expr::Print(Box::new(value.clone())),
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
    SleepFor,
    Print,
    Break,
    Continue,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
