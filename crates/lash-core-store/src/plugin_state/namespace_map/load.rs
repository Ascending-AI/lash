//! Source-admitted namespace reconstruction; every retained part owns its hold.
use super::*;
use crate::checkpoint_graph::admission::DecodeAdmission;

pub(super) fn load(
    source: &dyn NodeSource,
    root: NodeId,
    limits: DecodeLimits,
    budget: &AllocationBudget,
) -> Result<NamespaceBody, GraphRefusal> {
    let allocation = budget.reserve(1024)?;
    let graph = crate::checkpoint_graph::closure_in(source, root, &limits, budget)?;
    let mut decoder = GraphDecoder::new_in(&graph, limits, budget)?;
    let mut values = BTreeMap::new();
    let mut value_admission = DecodeAdmission::new_in(budget)?;
    value_admission.reserve(512)?;
    let mut prefix = Vec::new();
    let tree = load_trie(
        &mut decoder,
        &graph,
        root,
        &mut prefix,
        &mut values,
        &mut value_admission,
    )?;
    // Decoder buffers are private scratch. The JSON map and every trie node
    // already hold the obligations acquired before their own construction.
    drop(decoder);
    let bytes = graph
        .get(&root)
        .ok_or(GraphRefusal::MissingNode(root))?
        .bytes
        .clone();
    let published = crate::source_map::SharedSourceMap::empty_in(budget)?;
    Ok(NamespaceBody(Arc::new(NamespaceBodyStorage {
        values: BlobRef(root.to_hex()),
        bytes,
        graph: NamespaceNodes(graph),
        published: NamespaceNodes(published),
        snapshot: NamespaceValues::from_admitted(values, value_admission.finish(), budget.clone()),
        tree,
        budget: budget.clone(),
        _allocation: allocation,
    })))
}

fn load_trie(
    decoder: &mut crate::checkpoint_graph::AdmittedGraphDecoder<'_>,
    graph: &BTreeMap<NodeId, EncodedNode>,
    root: NodeId,
    prefix: &mut Vec<u8>,
    values: &mut BTreeMap<String, Value>,
    value_admission: &mut DecodeAdmission,
) -> Result<Arc<Trie>, GraphRefusal> {
    if prefix.len() > 128 {
        return Err(invalid("namespace key exceeds 128 bytes"));
    }
    let budget = value_admission.budget().clone();
    let mut admission = DecodeAdmission::new_in(&budget)?;
    admission.reserve(1024)?;
    let index = decoder.decode::<Body>(root, NodeKind::NAMESPACE_VALUE)?;
    let Body::Index(value, children) = &*index else {
        return Err(invalid("namespace index holds a value"));
    };
    if !prefix.is_empty() && value.is_none() && children.is_empty() {
        return Err(invalid("empty namespace branch"));
    }
    let value = value
        .map(|root| {
            let mut leaf_admission = DecodeAdmission::new_in(&budget)?;
            leaf_admission.reserve(1024)?;
            let decoded = decoder.decode::<Body>(root, NodeKind::NAMESPACE_VALUE)?;
            let Body::Value(bytes) = &*decoded else {
                return Err(invalid("namespace value holds an index"));
            };
            let capacity = lash_protocol_types::allocation::msgpack_decode_capacity(bytes, 512)
                .ok_or_else(|| invalid("malformed namespace MessagePack value"))?;
            value_admission.reserve(capacity.saturating_add(prefix.len() as u64 + 2048))?;
            let key =
                String::from_utf8(prefix.clone()).map_err(|error| invalid(error.to_string()))?;
            let value = rmp_serde::from_slice(bytes)
                .map_err(|error| invalid(format!("namespace value: {error}")))?;
            values.insert(key, value);
            let nodes = owned_nodes(graph, root, &BTreeSet::new(), &mut leaf_admission)?;
            Ok::<_, GraphRefusal>(Arc::new(Leaf {
                root,
                nodes,
                _allocation: leaf_admission.finish(),
            }))
        })
        .transpose()?;
    admission.reserve(
        (children.len() as u64)
            .saturating_mul(2048)
            .saturating_add(1024),
    )?;
    let mut loaded = BTreeMap::new();
    for (&byte, &child) in children {
        if prefix.len() == prefix.capacity() {
            value_admission
                .reserve((prefix.capacity().saturating_mul(2).max(8) as u64).saturating_add(128))?;
        }
        prefix.push(byte);
        let child = load_trie(decoder, graph, child, prefix, values, value_admission)?;
        prefix.pop();
        loaded.insert(byte, child);
    }
    admission.reserve((loaded.len() as u64 + 1).saturating_mul(1024))?;
    let boundaries = loaded
        .values()
        .map(|child| child.root)
        .chain(value.iter().map(|value| value.root))
        .collect();
    let nodes = owned_nodes(graph, root, &boundaries, &mut admission)?;
    let graph_nodes = loaded
        .values()
        .fold(nodes.len() as u64, |count, child| {
            count.saturating_add(child.graph_nodes)
        })
        .saturating_add(value.as_ref().map_or(0, |value| value.nodes.len() as u64));
    Ok(Arc::new(Trie {
        root,
        nodes,
        value,
        children: loaded,
        _allocation: admission.finish(),
        graph_nodes,
    }))
}

fn owned_nodes(
    graph: &BTreeMap<NodeId, EncodedNode>,
    root: NodeId,
    boundaries: &BTreeSet<NodeId>,
    admission: &mut DecodeAdmission,
) -> Result<BTreeMap<NodeId, EncodedNode>, GraphRefusal> {
    admission.reserve(512)?;
    let mut nodes = BTreeMap::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        if boundaries.contains(&id) || nodes.contains_key(&id) {
            continue;
        }
        let node = graph.get(&id).ok_or(GraphRefusal::MissingNode(id))?;
        admission.reserve(
            (node.bytes.len() as u64)
                .saturating_mul(64)
                .saturating_add(2048),
        )?;
        let children = node.children()?;
        admission.reserve(
            (children.len() as u64)
                .saturating_mul(128)
                .saturating_add(2048),
        )?;
        pending.extend(children);
        nodes.insert(id, node.clone());
    }
    Ok(nodes)
}

fn invalid(reason: impl Into<String>) -> GraphRefusal {
    GraphRefusal::InvalidBody {
        kind: NodeKind::NAMESPACE_VALUE.0,
        reason: reason.into(),
    }
}
