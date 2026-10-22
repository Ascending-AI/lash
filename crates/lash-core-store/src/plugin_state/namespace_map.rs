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

/// A resident optimization with no value semantics and no serialized state.
#[derive(Clone, Default)]
pub struct NamespaceEncoding(Arc<Mutex<Option<NamespaceBody>>>);

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
    pub(super) fn detached(&self) -> Self {
        Self(Arc::new(Mutex::new(
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )))
    }

    pub(super) fn body(&self, values: &NamespaceValues) -> NamespaceBody {
        let mut cache = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let body = NamespaceBody::update(Arc::clone(values), cache.as_ref());
        *cache = Some(body.clone());
        body
    }

    /// Seed the resident cache from a checked durable graph.
    pub fn recorded(body: NamespaceBody) -> Self {
        Self(Arc::new(Mutex::new(Some(body))))
    }
}

/// A namespace's complete keyed graph and the nodes its publication submits.
#[derive(Clone, Debug)]
pub struct NamespaceBody {
    pub values: BlobRef,
    /// Root envelope, carried as the checkpoint component's body.
    pub bytes: lash_kernel_doc::allocation::SharedBytes,
    /// Complete read closure. Node bodies are shared, never re-encoded.
    pub graph: BTreeMap<NodeId, EncodedNode>,
    /// Only nodes not held by the preceding durable root.
    pub published: BTreeMap<NodeId, EncodedNode>,
    snapshot: NamespaceValues,
    tree: Arc<Trie>,
}

impl PartialEq for NamespaceBody {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}
impl Eq for NamespaceBody {}

#[derive(Clone, Debug)]
struct Leaf {
    root: NodeId,
    nodes: BTreeMap<NodeId, EncodedNode>,
}

#[derive(Clone, Debug)]
struct Trie {
    root: NodeId,
    nodes: BTreeMap<NodeId, EncodedNode>,
    value: Option<Arc<Leaf>>,
    children: BTreeMap<u8, Arc<Trie>>,
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
    fn new(value: Option<Arc<Leaf>>, children: BTreeMap<u8, Arc<Self>>) -> Arc<Self> {
        let (root, nodes) = encode(&Body::Index(
            value.as_ref().map(|value| value.root),
            children
                .iter()
                .map(|(byte, child)| (*byte, child.root))
                .collect(),
        ));
        Arc::new(Self {
            root,
            nodes,
            value,
            children,
        })
    }

    fn change(tree: Option<&Arc<Self>>, key: &[u8], value: Option<Arc<Leaf>>) -> Option<Arc<Self>> {
        let mut children = tree.map(|tree| tree.children.clone()).unwrap_or_default();
        let mut held = tree.and_then(|tree| tree.value.clone());
        match key.split_first() {
            None => held = value,
            Some((byte, suffix)) => {
                let child = Self::change(children.get(byte), suffix, value);
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
            None
        } else {
            Some(Self::new(held, children))
        }
    }

    fn collect(&self, nodes: &mut BTreeMap<NodeId, EncodedNode>) {
        nodes.extend(self.nodes.clone());
        if let Some(value) = &self.value {
            nodes.extend(value.nodes.clone());
        }
        for child in self.children.values() {
            child.collect(nodes);
        }
    }
}

impl NamespaceBody {
    /// Encode an initial map. Mutations use the namespace's resident cache.
    #[must_use]
    pub fn encode(values: &BTreeMap<String, Value>) -> Self {
        Self::update(Arc::new(values.clone()), None)
    }

    #[expect(
        clippy::expect_used,
        reason = "serde_json values have a total MessagePack encoding"
    )]
    fn update(values: NamespaceValues, previous: Option<&Self>) -> Self {
        if let Some(previous) = previous.filter(|previous| Arc::ptr_eq(&previous.snapshot, &values))
        {
            return previous.clone();
        }
        let mut tree = previous.map(|previous| Arc::clone(&previous.tree));
        for (key, value) in values.iter() {
            if previous.is_some_and(|previous| previous.snapshot.get(key) == Some(value)) {
                continue;
            }
            let bytes = rmp_serde::to_vec_named(value).expect("namespace value encodes");
            let (root, nodes) = encode(&Body::Value(bytes));
            tree = Trie::change(
                tree.as_ref(),
                key.as_bytes(),
                Some(Arc::new(Leaf { root, nodes })),
            );
        }
        if let Some(previous) = previous {
            for key in previous
                .snapshot
                .keys()
                .filter(|key| !values.contains_key(*key))
            {
                tree = Trie::change(tree.as_ref(), key.as_bytes(), None);
            }
        }
        let tree = tree.unwrap_or_else(|| Trie::new(None, BTreeMap::new()));
        let mut graph = BTreeMap::new();
        tree.collect(&mut graph);
        Self {
            values: BlobRef(tree.root.to_hex()),
            bytes: graph[&tree.root].bytes.clone(),
            published: graph.clone(),
            graph,
            snapshot: values,
            tree,
        }
    }

    #[must_use]
    pub fn root(&self) -> NodeId {
        self.tree.root
    }

    #[must_use]
    pub fn snapshot(&self) -> NamespaceValues {
        Arc::clone(&self.snapshot)
    }

    /// Restrict publication to the new path. A pending predecessor carries
    /// its own uncommitted nodes forward; a committed predecessor holds all
    /// of its nodes durably. No speculative encoding implies durability.
    #[must_use]
    pub fn publication_after(&self, previous: Option<&Self>, committed: bool) -> Self {
        let mut body = self.clone();
        body.published.retain(|id, _| {
            !previous.is_some_and(|previous| {
                previous.graph.contains_key(id)
                    && (committed || !previous.published.contains_key(id))
            })
        });
        body
    }

    /// Exclude addresses retained by a committed head, without retaining
    /// that head's values or encoded payloads in resident state.
    #[must_use]
    pub fn publication_excluding(&self, held: Option<&BTreeSet<NodeId>>) -> Self {
        let mut body = self.clone();
        if let Some(held) = held {
            body.published.retain(|id, _| !held.contains(id));
        }
        body
    }

    #[must_use]
    pub fn value_address(&self, key: &str) -> Option<NodeId> {
        let mut tree = &self.tree;
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
        let graph = closure(source, root, &limits)?;
        let mut decoder = GraphDecoder::new(source, limits);
        let mut values = BTreeMap::new();
        let tree = load_trie(&mut decoder, &graph, root, &mut Vec::new(), &mut values)?;
        Ok(Self {
            values: BlobRef(root.to_hex()),
            bytes: graph[&root].bytes.clone(),
            graph,
            published: BTreeMap::new(),
            snapshot: Arc::new(values),
            tree,
        })
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
            Ok::<_, GraphRefusal>(Arc::new(Leaf { root, nodes }))
        })
        .transpose()?;
    let mut loaded = BTreeMap::new();
    for (byte, child) in children {
        prefix.push(byte);
        let child = load_trie(decoder, graph, child, prefix, values)?;
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
    Ok(Arc::new(Trie {
        root,
        nodes,
        value,
        children: loaded,
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
