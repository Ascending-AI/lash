//! A run's execution closure (FIG-6918): every library function its
//! document reaches, by identity, as one checkpoint graph node.
//!
//! Code is pinned by function identity only. A checkpoint root names the
//! closure of its document: each function the document lists and every
//! function their bodies reach, ascending by identity, each with its
//! definition node (kind 14) and whether only a native implements it. The
//! closure is derived from the document, so a worker and a parent derive one
//! node from one document, and a root carries only its reference.
//!
//! Whoever commits a run's first root stages the closure node and every
//! definition node it names beside it. The store retains them while a root
//! reaches them, equal closures are one node, and the parent reads which
//! identities a parked run needs without the binary that ran it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use lash_core_store::checkpoint_graph::stage::{CandidateManifest, ManifestEntry, StageFault};
use lash_core_store::checkpoint_graph::{
    EncodedNode, GraphRefusal, GraphStaging, NodeId, NodeKind, TypedBody,
};
use lash_kernel_doc::graph_tokens::{
    ChildRef, TokenCodec, TokenError, TokenReader, TokenWriter, check_ascending, encode_body,
};
use lash_kernel_doc::{FunctionId, FunctionRegistry, RegisteredFunction};
use lash_vm_protocol::KernelRestoreRefusal;

/// One function a closure holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosureFunction {
    pub function: FunctionId,
    /// Its HelperDefinition node.
    pub definition: ChildRef,
    /// Whether only a native implementation implements it: a resume needs
    /// that native, and a function with a body runs it without one
    /// (`K-LIB-002`).
    pub native_only: bool,
}

impl TokenCodec for ClosureFunction {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.function.encode(out);
        self.definition.encode(out);
        self.native_only.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            function: FunctionId::decode(input)?,
            definition: ChildRef::decode(input)?,
            native_only: bool::decode(input)?,
        })
    }
}

/// Kind 13's body: the functions a document reaches, ascending by identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutionClosure {
    functions: Vec<ClosureFunction>,
}

impl TokenCodec for ExecutionClosure {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(1);
        self.functions.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(1)?;
        let functions = Vec::<ClosureFunction>::decode(input)?;
        check_ascending(
            &functions
                .iter()
                .map(|function| function.function)
                .collect::<Vec<_>>(),
        )?;
        Ok(Self { functions })
    }
}

impl ExecutionClosure {
    /// Every function the closure holds, ascending by identity.
    #[must_use]
    pub fn functions(&self) -> &[ClosureFunction] {
        &self.functions
    }

    /// Every identity the closure holds.
    pub fn identities(&self) -> impl Iterator<Item = FunctionId> + '_ {
        self.functions.iter().map(|function| function.function)
    }
}

/// Why a document's closure is not derived over a registry: what a resume
/// of a run that pins it refuses, naming the function.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClosureRefusal {
    /// The registry holds no function of this identity.
    #[error("function {function} is not held here")]
    Missing { function: FunctionId },
    /// Only a native implements the function, and the registry has none.
    #[error("function {function} needs a native implementation this worker lacks")]
    NativeMissing { function: FunctionId },
    /// A definition did not encode as a graph node: a defect of the build.
    #[error(transparent)]
    Graph(#[from] GraphRefusal),
}

impl From<ClosureRefusal> for KernelRestoreRefusal {
    fn from(refusal: ClosureRefusal) -> Self {
        match refusal {
            ClosureRefusal::Missing { function } => Self::MissingFunction {
                function: function.to_string(),
            },
            ClosureRefusal::NativeMissing { function } => Self::MissingNative {
                function: function.to_string(),
            },
            ClosureRefusal::Graph(refusal) => Self::Malformed {
                problem: refusal.to_string(),
            },
        }
    }
}

/// Whether a closure is derived for a machine that runs it, which holds the
/// native of every native-only function, or for a parent that only links.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Natives {
    /// A worker's registry: a native-only function without its native is
    /// refused.
    Required,
    /// A parent's declarations: natives are a worker's.
    Declared,
}

/// A document's execution closure, with the nodes that store it.
#[derive(Clone, Debug)]
pub struct Closure {
    reference: NodeId,
    body: Arc<ExecutionClosure>,
    /// The closure node and every node of every definition it names.
    nodes: Arc<BTreeMap<NodeId, EncodedNode>>,
}

impl Closure {
    /// The closure node: what a root names.
    #[must_use]
    pub fn reference(&self) -> NodeId {
        self.reference
    }

    /// The closure as its node holds it.
    #[must_use]
    pub fn body(&self) -> &ExecutionClosure {
        &self.body
    }

    /// The closure node and every definition node it reaches.
    #[must_use]
    pub fn nodes(&self) -> &BTreeMap<NodeId, EncodedNode> {
        &self.nodes
    }

    /// Stages the closure in `tx`, whose root names it: every node on the
    /// `first` root of an execution, which the store may already hold as
    /// equal nodes, and a requirement of the stored closure on every later
    /// one, which the execution's earlier root keeps reachable.
    pub fn stage(&self, tx: &mut lash_durable::ActorTx, first: bool) {
        if first {
            tx.stage(self.nodes.values().cloned());
        } else {
            tx.require(self.reference);
        }
    }
}

/// Stages in `tx` each of `closures` that `manifest` names, and answers
/// their references. The pages a commit publishes may name the closure of
/// an earlier cell of the session, through a function that cell saved,
/// which no commit has staged yet.
///
/// # Errors
///
/// [`StageFault`]: the manifest did not read.
pub fn stage_named<'a>(
    manifest: &mut dyn CandidateManifest,
    closures: impl IntoIterator<Item = &'a Closure>,
    tx: &mut lash_durable::ActorTx,
) -> Result<BTreeSet<NodeId>, StageFault> {
    const BATCH: usize = 1024;
    let closures: BTreeMap<NodeId, &Closure> = closures
        .into_iter()
        .map(|closure| (closure.reference(), closure))
        .collect();
    let mut staged = BTreeSet::new();
    if closures.is_empty() {
        return Ok(staged);
    }
    let mut after = None;
    loop {
        let entries = manifest.entries(after, BATCH)?;
        let Some(last) = entries.last() else {
            return Ok(staged);
        };
        after = Some(match last {
            ManifestEntry::External(id) => *id,
            ManifestEntry::Fresh(node) => node.id,
        });
        for entry in entries {
            if let ManifestEntry::External(id) = entry
                && let Some(closure) = closures.get(&id)
                && staged.insert(id)
            {
                closure.stage(tx, true);
            }
        }
    }
}

/// One definition's nodes, encoded once in a process.
#[derive(Debug)]
struct DefinitionNodes {
    id: NodeId,
    nodes: Vec<EncodedNode>,
}

fn staging() -> GraphStaging {
    use lash_core_store::checkpoint_graph::{EncodeBudget, ExplicitLimit};
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)))
}

/// `registered`'s definition as its graph nodes, encoded once per process:
/// its identity names one definition, so its nodes never change.
fn definition_nodes(registered: &RegisteredFunction) -> Result<Arc<DefinitionNodes>, GraphRefusal> {
    static NODES: OnceLock<Mutex<HashMap<FunctionId, Arc<DefinitionNodes>>>> = OnceLock::new();
    let cache = NODES.get_or_init(Mutex::default);
    let function = registered.identity();
    if let Some(nodes) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&function)
    {
        return Ok(Arc::clone(nodes));
    }
    let mut staging = staging();
    let id = staging
        .encode(
            NodeKind::HELPER_DEFINITION,
            TypedBody::logical(&registered.image()),
        )
        .map_err(GraphRefusal::from)?;
    let nodes = Arc::new(DefinitionNodes {
        id,
        nodes: staging.into_nodes().into_values().collect(),
    });
    Ok(Arc::clone(
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(function)
            .or_insert(nodes),
    ))
}

/// The closure of `listed`, the library functions a document lists, over
/// `functions`: each listed function and every function a body it holds
/// reaches, transitively.
///
/// # Errors
///
/// [`ClosureRefusal`], naming the first function, by identity, that
/// `functions` does not hold, or whose native it lacks under
/// [`Natives::Required`].
pub fn derive(
    listed: impl IntoIterator<Item = FunctionId>,
    functions: &FunctionRegistry,
    natives: Natives,
) -> Result<Closure, ClosureRefusal> {
    let mut reached = BTreeSet::new();
    let mut pending: Vec<FunctionId> = listed.into_iter().collect();
    while let Some(function) = pending.pop() {
        if reached.contains(&function) {
            continue;
        }
        let registered = functions
            .get(&function)
            .ok_or(ClosureRefusal::Missing { function })?;
        pending.extend(registered.callees().iter().copied());
        reached.insert(function);
    }
    let mut body = ExecutionClosure::default();
    let mut nodes = BTreeMap::new();
    for function in reached {
        let registered = functions
            .get(&function)
            .ok_or(ClosureRefusal::Missing { function })?;
        let native_only = registered.states_native() && !registered.has_body();
        if native_only && natives == Natives::Required && registered.native().is_none() {
            return Err(ClosureRefusal::NativeMissing { function });
        }
        let definition = definition_nodes(registered)?;
        nodes.extend(definition.nodes.iter().map(|node| (node.id, node.clone())));
        body.functions.push(ClosureFunction {
            function,
            definition: definition.id.child_ref(),
            native_only,
        });
    }
    let mut staging = staging();
    let reference = staging
        .encode(
            NodeKind::EXECUTION_CLOSURE,
            TypedBody::logical(&encode_body(&body)),
        )
        .map_err(GraphRefusal::from)?;
    nodes.extend(staging.into_nodes());
    Ok(Closure {
        reference,
        body: Arc::new(body),
        nodes: Arc::new(nodes),
    })
}

#[cfg(test)]
#[path = "closure_tests.rs"]
mod tests;
