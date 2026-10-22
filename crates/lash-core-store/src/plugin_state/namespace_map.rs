//! Persistent byte trie for namespace keys. Values have independent content
//! addresses; every index node names only its immediate children. The JSON
//! map remains the resident read view, not the durable encoding.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::NamespaceValues;
use crate::checkpoint_graph::{
    DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphDecoder, GraphRefusal,
    GraphStaging, NodeId, NodeKind, NodeSource, TokenCodec, TokenError, TokenReader, TokenWriter,
    closure,
};
use crate::store::BlobRef;
use lash_protocol_types::allocation::{
    AllocationBudget, AllocationError, AllocationHold, payload_capacity,
};

/// A resident optimization with no value semantics and no serialized state.
#[derive(Clone, Default)]
pub struct NamespaceEncoding(Arc<EncodingSource>);
#[derive(Default)]
struct EncodingSource {
    cache: Mutex<Option<NamespaceBody>>,
    _allocation: AllocationHold,
    budget: AllocationBudget,
}

impl std::fmt::Debug for NamespaceEncoding {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("NamespaceEncoding")
    }
}
impl PartialEq for NamespaceEncoding {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}
impl Eq for NamespaceEncoding {}

impl NamespaceEncoding {
    pub(super) fn decoded_in(
        &mut self,
        allocation: AllocationHold,
        budget: AllocationBudget,
    ) -> bool {
        let Some(storage) = Arc::get_mut(&mut self.0) else {
            return false;
        };
        storage._allocation = allocation;
        storage.budget = budget;
        true
    }

    pub(super) fn empty_in(budget: &AllocationBudget) -> Result<Self, AllocationError> {
        let allocation = budget.reserve(256)?;
        Ok(Self(Arc::new(EncodingSource {
            cache: Mutex::new(None),
            _allocation: allocation,
            budget: budget.clone(),
        })))
    }

    pub(super) fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        if self.0.budget.identity() == budget.identity() {
            return Ok(self.clone());
        }
        let allocation = budget.reserve(256)?;
        Ok(Self(Arc::new(EncodingSource {
            cache: Mutex::new(None),
            _allocation: allocation,
            budget: budget.clone(),
        })))
    }

    pub(super) fn detached(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        let allocation = budget.reserve(256)?;
        let cache = self
            .0
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok(Self(Arc::new(EncodingSource {
            cache: Mutex::new(cache),
            _allocation: allocation,
            budget: budget.clone(),
        })))
    }

    pub(super) fn body(&self, values: &NamespaceValues) -> Result<NamespaceBody, AllocationError> {
        let mut cache = self
            .0
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let body = NamespaceBody::update(values.clone(), cache.as_ref())?;
        *cache = Some(body.clone());
        Ok(body)
    }

    /// Seed the resident cache from a checked durable graph.
    pub fn recorded(body: NamespaceBody) -> Result<Self, AllocationError> {
        let budget = body.0.budget.clone();
        let allocation = budget.reserve(256)?;
        Ok(Self(Arc::new(EncodingSource {
            cache: Mutex::new(Some(body)),
            _allocation: allocation,
            budget: budget.clone(),
        })))
    }
}

/// A namespace's complete keyed graph and the nodes its publication submits.
#[derive(Clone, Debug)]
pub struct NamespaceBody(Arc<NamespaceBodyStorage>);

/// Immutable source shared by every resident namespace body and publication view.
#[derive(Clone, Debug)]
struct NamespaceBodyStorage {
    pub values: BlobRef,
    /// Root envelope, carried as the checkpoint component's body.
    pub bytes: lash_kernel_doc::allocation::SharedBytes,
    /// Complete read closure. Node bodies are shared, never re-encoded.
    graph: NamespaceNodes,
    /// Only nodes not held by the preceding durable root.
    published: NamespaceNodes,
    snapshot: NamespaceValues,
    tree: Arc<Trie>,
    budget: AllocationBudget,
    _allocation: AllocationHold,
}

#[derive(Clone, Debug)]
struct NamespaceNodes(Arc<NamespaceNodeMap>);
#[derive(Debug)]
struct NamespaceNodeMap {
    nodes: BTreeMap<NodeId, EncodedNode>,
    _allocation: AllocationHold,
}
impl NamespaceNodes {
    fn new(nodes: BTreeMap<NodeId, EncodedNode>, allocation: AllocationHold) -> Self {
        Self(Arc::new(NamespaceNodeMap {
            nodes,
            _allocation: allocation,
        }))
    }
    fn capacity(&self) -> u64 {
        (self.0.nodes.len() as u64)
            .saturating_mul(2048)
            .saturating_add(512)
    }
}

impl PartialEq for NamespaceBody {
    fn eq(&self, other: &Self) -> bool {
        self.0.values == other.0.values
    }
}
impl Eq for NamespaceBody {}

#[derive(Clone, Debug)]
struct Leaf {
    root: NodeId,
    nodes: BTreeMap<NodeId, EncodedNode>,
    _allocation: AllocationHold,
}

#[derive(Clone, Debug)]
struct Trie {
    root: NodeId,
    nodes: BTreeMap<NodeId, EncodedNode>,
    value: Option<Arc<Leaf>>,
    children: BTreeMap<u8, Arc<Trie>>,
    _allocation: AllocationHold,
    graph_nodes: u64,
}

// Kind 35 is a keyed namespace graph: value, index, or plugin-owned root.
// The durable crate owns the plugin-owned root (variant 2).
enum Body {
    Value(Vec<u8>),
    Index(Option<NodeId>, BTreeMap<u8, NodeId>),
}

impl TokenCodec for Body {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Value(bytes) => {
                out.sum(0, 1);
                out.bytes(bytes);
            }
            Self::Index(value, children) => {
                out.sum(1, 2);
                value.encode(out);
                out.seq(children.len() as u64);
                for (byte, child) in children {
                    out.seq(2);
                    out.unsigned(u64::from(*byte));
                    child.encode(out);
                }
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        match variant {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                Ok(Self::Value(input.bytes()?.to_vec()))
            }
            1 => {
                TokenReader::expect_fields(fields, 2)?;
                let value = Option::<NodeId>::decode(input)?;
                let count = input.seq()?;
                let mut children = BTreeMap::new();
                for _ in 0..count {
                    input.expect_struct(2)?;
                    let byte =
                        u8::try_from(input.unsigned()?).map_err(|_| TokenError::LengthOverflow)?;
                    if children
                        .keys()
                        .next_back()
                        .is_some_and(|last| *last >= byte)
                    {
                        return Err(TokenError::LengthOverflow);
                    }
                    children.insert(byte, NodeId::decode(input)?);
                }
                Ok(Self::Index(value, children))
            }
            variant => Err(TokenError::UnsupportedVariant { variant }),
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "typed namespace bodies encode within an unbounded staging budget"
)]
fn encode(body: &Body) -> (NodeId, BTreeMap<NodeId, EncodedNode>) {
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let root = staging
        .encode_value(NodeKind::NAMESPACE_VALUE, body)
        .expect("namespace graph encodes");
    (root, staging.into_nodes())
}

impl Trie {
    fn new(
        value: Option<Arc<Leaf>>,
        children: BTreeMap<u8, Arc<Self>>,
        allocation: AllocationHold,
    ) -> Arc<Self> {
        let (root, nodes) = encode(&Body::Index(
            value.as_ref().map(|value| value.root),
            children
                .iter()
                .map(|(byte, child)| (*byte, child.root))
                .collect(),
        ));
        let graph_nodes = children
            .values()
            .fold(nodes.len() as u64, |count, child| {
                count.saturating_add(child.graph_nodes)
            })
            .saturating_add(value.as_ref().map_or(0, |value| value.nodes.len() as u64));
        Arc::new(Self {
            root,
            nodes,
            value,
            children,
            _allocation: allocation,
            graph_nodes,
        })
    }

    fn change(
        tree: Option<&Arc<Self>>,
        key: &[u8],
        value: Option<Arc<Leaf>>,
        budget: &AllocationBudget,
    ) -> Result<Option<Arc<Self>>, AllocationError> {
        // A sparse B-tree can allocate a full node for one entry. Cover the
        // copied child map and temporary index/encoding buffers before either.
        let allocation = budget.reserve(
            (tree.map_or(0, |tree| tree.children.len()) as u64 + 1)
                .saturating_mul(4096)
                .saturating_add(8192),
        )?;
        let mut children = tree.map(|tree| tree.children.clone()).unwrap_or_default();
        let mut held = tree.and_then(|tree| tree.value.clone());
        match key.split_first() {
            None => held = value,
            Some((byte, suffix)) => {
                let child = Self::change(children.get(byte), suffix, value, budget)?;
                match child {
                    Some(child) => {
                        children.insert(*byte, child);
                    }
                    None => {
                        children.remove(byte);
                    }
                }
            }
        }
        if held.is_none() && children.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Self::new(held, children, allocation)))
        }
    }

    fn collect(&self, nodes: &mut BTreeMap<NodeId, EncodedNode>) {
        nodes.extend(self.nodes.iter().map(|(id, node)| (*id, node.clone())));
        if let Some(value) = &self.value {
            nodes.extend(value.nodes.iter().map(|(id, node)| (*id, node.clone())));
        }
        for child in self.children.values() {
            child.collect(nodes);
        }
    }
}

impl NamespaceBody {
    pub fn clone_in(&self, budget: &AllocationBudget) -> Result<Self, AllocationError> {
        if self.0.budget.identity() == budget.identity() {
            return Ok(self.clone());
        }
        Self::update(self.0.snapshot.clone_in(budget)?, None)
    }

    pub fn values(&self) -> &BlobRef {
        &self.0.values
    }
    pub fn bytes(&self) -> &lash_kernel_doc::allocation::SharedBytes {
        &self.0.bytes
    }
    pub fn graph(&self) -> &BTreeMap<NodeId, EncodedNode> {
        &self.0.graph.0.nodes
    }
    pub fn published(&self) -> &BTreeMap<NodeId, EncodedNode> {
        &self.0.published.0.nodes
    }

    /// Encode an initial map. Mutations use the namespace's resident cache.
    #[must_use]
    pub fn encode(
        values: &BTreeMap<String, Value>,
        budget: &AllocationBudget,
    ) -> Result<Self, AllocationError> {
        Self::update(NamespaceValues::copy_from_map_in(values, budget)?, None)
    }

    #[expect(
        clippy::expect_used,
        reason = "serde_json values have a total MessagePack encoding"
    )]
    fn update(values: NamespaceValues, previous: Option<&Self>) -> Result<Self, AllocationError> {
        if let Some(previous) = previous.filter(|previous| previous.0.snapshot.ptr_eq(&values)) {
            return Ok(previous.clone());
        }
        let budget = values.allocation_budget().clone();
        let allocation = budget.reserve(1024)?;
        let mut tree = previous.map(|previous| Arc::clone(&previous.0.tree));
        for (key, value) in values.iter() {
            if previous.is_some_and(|previous| previous.0.snapshot.get(key) == Some(value)) {
                continue;
            }
            let leaf_allocation = budget.reserve(
                payload_capacity(value, 64)
                    .saturating_mul(16)
                    .saturating_add(8192),
            )?;
            let bytes = rmp_serde::to_vec_named(value).expect("namespace value encodes");
            let (root, nodes) = encode(&Body::Value(bytes));
            tree = Trie::change(
                tree.as_ref(),
                key.as_bytes(),
                Some(Arc::new(Leaf {
                    root,
                    nodes,
                    _allocation: leaf_allocation,
                })),
                &budget,
            )?;
        }
        if let Some(previous) = previous {
            for key in previous
                .0
                .snapshot
                .keys()
                .filter(|key| !values.contains_key(*key))
            {
                tree = Trie::change(tree.as_ref(), key.as_bytes(), None, &budget)?;
            }
        }
        let tree = match tree {
            Some(tree) => tree,
            None => Trie::new(None, BTreeMap::new(), budget.reserve(12288)?),
        };
        let graph_allocation =
            budget.reserve(tree.graph_nodes.saturating_mul(2048).saturating_add(512))?;
        let mut graph = BTreeMap::new();
        tree.collect(&mut graph);
        let bytes = graph[&tree.root].bytes.clone();
        let graph = NamespaceNodes::new(graph, graph_allocation);
        Ok(Self(Arc::new(NamespaceBodyStorage {
            values: BlobRef(tree.root.to_hex()),
            bytes,
            published: graph.clone(),
            graph,
            snapshot: values,
            tree,
            budget,
            _allocation: allocation,
        })))
    }

    #[must_use]
    pub fn root(&self) -> NodeId {
        self.0.tree.root
    }

    #[must_use]
    pub fn snapshot(&self) -> NamespaceValues {
        self.0.snapshot.clone()
    }

    /// Restrict publication to the new path. A pending predecessor carries
    /// its own uncommitted nodes forward; a committed predecessor holds all
    /// of its nodes durably. No speculative encoding implies durability.
    #[must_use]
    pub fn publication_after(
        &self,
        previous: Option<&Self>,
        committed: bool,
    ) -> Result<Self, AllocationError> {
        self.publication_filter(|id| {
            !previous.is_some_and(|previous| {
                previous.graph().contains_key(id)
                    && (committed || !previous.published().contains_key(id))
            })
        })
    }

    /// Exclude addresses held by the committed root, reserving the new view first.
    pub fn publication_excluding(
        &self,
        held: Option<&crate::source_map::SharedSourceMap<NodeId, ()>>,
    ) -> Result<Self, AllocationError> {
        self.publication_filter(|id| !held.is_some_and(|held| held.contains_key(id)))
    }

    fn publication_filter(&self, keep: impl Fn(&NodeId) -> bool) -> Result<Self, AllocationError> {
        let allocation = self.0.budget.reserve(self.0.published.capacity())?;
        let published = self
            .published()
            .iter()
            .filter(|(id, _)| keep(id))
            .map(|(id, node)| (*id, node.clone()))
            .collect();
        let mut body = (*self.0).clone();
        body.published = NamespaceNodes::new(published, allocation.clone());
        body._allocation = allocation;
        Ok(Self(Arc::new(body)))
    }

    #[must_use]
    pub fn value_address(&self, key: &str) -> Option<NodeId> {
        let mut tree = &self.0.tree;
        for byte in key.bytes() {
            tree = tree.children.get(&byte)?;
        }
        tree.value.as_ref().map(|value| value.root)
    }

    /// Read and check the keyed map under a durable root.
    ///
    /// # Errors
    /// Any graph refusal, or a malformed trie or JSON value.
    pub fn load(
        source: &dyn NodeSource,
        root: NodeId,
        limits: DecodeLimits,
    ) -> Result<Self, GraphRefusal> {
        let allocation = AllocationHold::default();
        let graph = closure(source, root, &limits)?;
        let mut decoder = GraphDecoder::new(source, limits);
        let mut values = BTreeMap::new();
        let tree = load_trie(
            &mut decoder,
            &graph,
            root,
            &mut Vec::new(),
            &mut values,
            &allocation,
        )?;
        Ok(Self(Arc::new(NamespaceBodyStorage {
            values: BlobRef(root.to_hex()),
            bytes: graph[&root].bytes.clone(),
            graph: NamespaceNodes::new(graph, allocation.clone()),
            published: NamespaceNodes::new(BTreeMap::new(), allocation.clone()),
            snapshot: values.into(),
            tree,
            budget: AllocationBudget::unowned(),
            _allocation: allocation,
        })))
    }
}

fn invalid(reason: impl Into<String>) -> GraphRefusal {
    GraphRefusal::InvalidBody {
        kind: NodeKind::NAMESPACE_VALUE.0,
        reason: reason.into(),
    }
}

fn load_trie(
    decoder: &mut GraphDecoder<'_>,
    graph: &BTreeMap<NodeId, EncodedNode>,
    root: NodeId,
    prefix: &mut Vec<u8>,
    values: &mut BTreeMap<String, Value>,
    allocation: &AllocationHold,
) -> Result<Arc<Trie>, GraphRefusal> {
    if prefix.len() > 128 {
        return Err(invalid("namespace key exceeds 128 bytes"));
    }
    let Body::Index(value, children) = decoder.decode(root, NodeKind::NAMESPACE_VALUE)? else {
        return Err(invalid("namespace index holds a value"));
    };
    if !prefix.is_empty() && value.is_none() && children.is_empty() {
        return Err(invalid("empty namespace branch"));
    }
    let value = value
        .map(|root| {
            let Body::Value(bytes) = decoder.decode(root, NodeKind::NAMESPACE_VALUE)? else {
                return Err(invalid("namespace value holds an index"));
            };
            let key =
                String::from_utf8(prefix.clone()).map_err(|error| invalid(error.to_string()))?;
            let value = rmp_serde::from_slice(&bytes)
                .map_err(|error| invalid(format!("namespace value: {error}")))?;
            values.insert(key, value);
            let nodes = owned_nodes(graph, root, &BTreeSet::new())?;
            Ok::<_, GraphRefusal>(Arc::new(Leaf {
                root,
                nodes,
                _allocation: allocation.clone(),
            }))
        })
        .transpose()?;
    let mut loaded = BTreeMap::new();
    for (byte, child) in children {
        prefix.push(byte);
        let child = load_trie(decoder, graph, child, prefix, values, allocation)?;
        prefix.pop();
        loaded.insert(byte, child);
    }
    // Keep this index's placed/streamed structure, stopping at the logical
    // namespace children. The initial closure checked every envelope once.
    let boundaries = loaded
        .values()
        .map(|child| child.root)
        .chain(value.iter().map(|value| value.root))
        .collect();
    let nodes = owned_nodes(graph, root, &boundaries)?;
    let graph_nodes = loaded
        .values()
        .fold(nodes.len() as u64, |count, child: &Arc<Trie>| {
            count.saturating_add(child.graph_nodes)
        })
        .saturating_add(value.as_ref().map_or(0, |value| value.nodes.len() as u64));
    Ok(Arc::new(Trie {
        root,
        nodes,
        value,
        children: loaded,
        _allocation: allocation.clone(),
        graph_nodes,
    }))
}

fn owned_nodes(
    graph: &BTreeMap<NodeId, EncodedNode>,
    root: NodeId,
    boundaries: &BTreeSet<NodeId>,
) -> Result<BTreeMap<NodeId, EncodedNode>, GraphRefusal> {
    let mut nodes = BTreeMap::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        if boundaries.contains(&id) || nodes.contains_key(&id) {
            continue;
        }
        let node = graph.get(&id).ok_or(GraphRefusal::MissingNode(id))?;
        pending.extend(node.children()?);
        nodes.insert(id, node.clone());
    }
    Ok(nodes)
}
