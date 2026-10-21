//! A run's execution closure (FIG-6918): every library function its
//! document reaches, by identity, as one checkpoint graph node.
//!
//! Code is pinned by function identity only. A checkpoint root names the
//! closure of its document: each function the document lists and every
//! function their bodies reach, ascending by identity, each with its
//! definition node (kind 14) and whether only a native implements it. The
//! closure is a function of the document and those definitions alone, so
//! a worker and a parent derive one node from one document, and a root
//! carries only its reference.
//!
//! A definition is the node's own when its registry holds that identity,
//! and is read from the store otherwise (FIG-6985): pinned work runs from
//! the definitions it was written against, whichever build runs it. A
//! closure carries the images of the stored ones, for the worker that
//! admits them ([`crate::kernel::loader`]).
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
use lash_kernel_doc::{FunctionDefinition, FunctionRegistry};
use lash_protocol_types::engine_fault::KernelRestoreRefusal;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::tokens::{
    ChildRef, TokenCodec, TokenError, TokenReader, TokenWriter, check_ascending, encode_body,
};

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
    /// Neither the registry nor the store holds a function of this
    /// identity.
    #[error("function {function} is neither held here nor stored")]
    Missing { function: FunctionId },
    /// A definition did not encode as a graph node: a defect of the build.
    #[error(transparent)]
    Graph(#[from] GraphRefusal),
}

impl From<ClosureRefusal> for KernelRestoreRefusal {
    fn from(refusal: ClosureRefusal) -> Self {
        match refusal {
            ClosureRefusal::Missing { function } => Self::DefinitionRefused {
                function: function.to_string(),
                problem: "it is neither held here nor stored".to_owned(),
            },
            ClosureRefusal::Graph(refusal) => Self::Malformed {
                problem: refusal.to_string(),
            },
        }
    }
}

/// A document's execution closure, with the nodes that store it.
#[derive(Clone, Debug)]
pub struct Closure {
    reference: NodeId,
    body: Arc<ExecutionClosure>,
    /// The closure node and every node of every definition it names.
    nodes: Arc<BTreeMap<NodeId, EncodedNode>>,
    /// The token body of each definition read from the store: those the
    /// registry it was derived over does not hold.
    stored: Arc<BTreeMap<FunctionId, Arc<[u8]>>>,
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

    /// The token body of each of its definitions that was read from the
    /// store, by identity: what a worker of this node lacks.
    #[must_use]
    pub fn stored(&self) -> &BTreeMap<FunctionId, Arc<[u8]>> {
        &self.stored
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

/// The definition `function`, whose token body `image` answers, as its
/// graph nodes, encoded once per process: its identity names one
/// definition, so its nodes never change.
fn definition_nodes(
    function: FunctionId,
    image: impl FnOnce() -> Arc<[u8]>,
) -> Result<Arc<DefinitionNodes>, GraphRefusal> {
    static NODES: OnceLock<Mutex<HashMap<FunctionId, Arc<DefinitionNodes>>>> = OnceLock::new();
    let cache = NODES.get_or_init(Mutex::default);
    if let Some(nodes) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&function)
    {
        return Ok(Arc::clone(nodes));
    }
    let mut staging = staging();
    let id = staging
        .encode(NodeKind::HELPER_DEFINITION, TypedBody::logical(&image()))
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

/// One function of a closure, as the registry holds it or the store
/// answered it.
enum Member<'a> {
    Held(&'a lash_kernel_doc::RegisteredFunction),
    Stored(&'a Arc<FunctionDefinition>),
}

impl Member<'_> {
    fn of<'a>(
        function: FunctionId,
        held: &'a FunctionRegistry,
        stored: &'a BTreeMap<FunctionId, Arc<FunctionDefinition>>,
    ) -> Result<Member<'a>, ClosureRefusal> {
        match (held.get(&function), stored.get(&function)) {
            (Some(registered), _) => Ok(Member::Held(registered)),
            (None, Some(definition)) => Ok(Member::Stored(definition)),
            (None, None) => Err(ClosureRefusal::Missing { function }),
        }
    }

    fn callees(&self) -> Vec<FunctionId> {
        match self {
            Member::Held(registered) => registered.callees().iter().copied().collect(),
            Member::Stored(definition) => definition
                .body()
                .map(|body| body.functions.keys().copied().collect())
                .unwrap_or_default(),
        }
    }

    fn native_only(&self) -> bool {
        match self {
            Member::Held(registered) => registered.states_native() && !registered.has_body(),
            Member::Stored(definition) => definition.has_native() && definition.body().is_none(),
        }
    }
}

/// The closure of `listed`, the library functions a document lists: each
/// listed function and every function a body it holds reaches,
/// transitively, each from `held`, the node's registry, or, when it holds
/// no such identity, from `stored`, the definitions read from the store.
///
/// # Errors
///
/// [`ClosureRefusal::Missing`], naming the first function, by identity,
/// that neither holds.
pub fn derive(
    listed: impl IntoIterator<Item = FunctionId>,
    held: &FunctionRegistry,
    stored: &BTreeMap<FunctionId, Arc<FunctionDefinition>>,
) -> Result<Closure, ClosureRefusal> {
    let mut reached = BTreeSet::new();
    let mut pending: Vec<FunctionId> = listed.into_iter().collect();
    while let Some(function) = pending.pop() {
        if reached.contains(&function) {
            continue;
        }
        pending.extend(Member::of(function, held, stored)?.callees());
        reached.insert(function);
    }
    let mut body = ExecutionClosure::default();
    let mut nodes = BTreeMap::new();
    let mut images = BTreeMap::new();
    for function in reached {
        let member = Member::of(function, held, stored)?;
        let definition = match &member {
            Member::Held(registered) => definition_nodes(function, || registered.image())?,
            Member::Stored(definition) => {
                let image: Arc<[u8]> = encode_body(definition.as_ref()).into();
                images.insert(function, Arc::clone(&image));
                definition_nodes(function, || image)?
            }
        };
        nodes.extend(definition.nodes.iter().map(|node| (node.id, node.clone())));
        body.functions.push(ClosureFunction {
            function,
            definition: definition.id.child_ref(),
            native_only: member.native_only(),
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
        stored: Arc::new(images),
    })
}
