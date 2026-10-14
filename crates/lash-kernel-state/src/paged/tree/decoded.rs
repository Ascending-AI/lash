//! Decoded cells retain bounded normal-form caches across repeated reads.
use super::*;
use std::any::{Any, TypeId};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Location {
    Page(PageId),
    Inline(usize),
}
type CacheKey = (Location, TypeId, Option<FamilyId>, bool);
struct Decoded {
    node: Box<dyn Any>,
    summary: Summary,
    _inline: Option<Arc<[u8]>>,
}
#[derive(Default)]
pub(super) struct DecodedResidency {
    nodes: BTreeMap<CacheKey, Decoded>,
    pub(super) bytes: usize,
}
impl Access<'_> {
    pub(super) fn decoded_node<T: Cell>(
        &self,
        page: PageRef,
        family: Option<FamilyId>,
        sequence: bool,
    ) -> Option<(Node<T>, Summary)> {
        let cache = self.decoded.borrow();
        let entry =
            cache
                .nodes
                .get(&(Location::Page(page.id), TypeId::of::<T>(), family, sequence))?;
        Some((entry.node.downcast_ref::<Node<T>>()?.clone(), entry.summary))
    }
    pub(super) fn retain_decoded<T: Cell>(
        &self,
        page: PageRef,
        family: Option<FamilyId>,
        sequence: bool,
        node: &Node<T>,
        summary: Summary,
    ) {
        let weight = node_weight(node);
        if *self.bytes.borrow() + self.decoded.borrow().bytes + weight > 64 << 20 {
            self.clear();
        }
        let mut cache = self.decoded.borrow_mut();
        cache.bytes += weight;
        cache.nodes.insert(
            (Location::Page(page.id), TypeId::of::<T>(), family, sequence),
            Decoded {
                node: Box::new(node.clone()),
                summary,
                _inline: None,
            },
        );
    }
    pub(super) fn inline_node<T: Cell>(
        &self,
        branch: &Branch<T>,
        tokens: &Arc<[u8]>,
        children: &[PageRef],
        sequence: bool,
    ) -> Result<Node<T>, HeapFault> {
        let key = (
            Location::Inline(tokens.as_ptr() as usize),
            TypeId::of::<T>(),
            branch.family,
            sequence,
        );
        if let Some(entry) = self.decoded.borrow().nodes.get(&key) {
            if entry.summary != branch.summary {
                return Err(HeapFault::Malformed(
                    "inline tree aggregate mismatch".into(),
                ));
            }
            if let Some(node) = entry.node.downcast_ref::<Node<T>>() {
                return Ok(node.clone());
            }
        }
        let node = Tree::inline_node(branch, tokens, children, true)?;
        let weight = node_weight(&node) + tokens.len();
        if *self.bytes.borrow() + self.decoded.borrow().bytes + weight > 64 << 20 {
            self.clear();
        }
        let mut cache = self.decoded.borrow_mut();
        cache.bytes += weight;
        cache.nodes.insert(
            key,
            Decoded {
                node: Box::new(node.clone()),
                summary: branch.summary,
                _inline: Some(tokens.clone()),
            },
        );
        Ok(node)
    }
    #[cfg(test)]
    pub(crate) fn clean_bytes(&self) -> usize {
        *self.bytes.borrow() + self.decoded.borrow().bytes
    }
}
/// Conservative physical allowance for cell allocations and their bounded
/// runtime caches. Nested trees retain links; their pages account separately.
pub(super) fn node_weight<T: Cell>(node: &Node<T>) -> usize {
    std::mem::size_of::<Node<T>>()
        + 128
        + match node {
            Node::Leaf(rows) => {
                rows.capacity() * std::mem::size_of::<(Key, T)>()
                    + rows
                        .iter()
                        .map(|(k, v)| k.0.capacity() + encode_body(v).len() * 16 + 512)
                        .sum::<usize>()
            }
            Node::Index(rows) => {
                rows.capacity() * std::mem::size_of::<Branch<T>>()
                    + rows.iter().map(|r| r.first.0.capacity()).sum::<usize>()
            }
            Node::Staged(..) => 0,
        }
}
pub(super) fn validate_summary<T: Cell>(
    branch: &Branch<T>,
    summary: Summary,
    page: PageRef,
) -> Result<(), HeapFault> {
    if summary.len != branch.summary.len || summary.cost != branch.summary.cost {
        return Err(corrupt(page, "tree aggregate mismatch"));
    }
    if branch.tasks_known && summary.tasks != branch.summary.tasks {
        return Err(corrupt(page, "task-edge aggregate mismatch"));
    }
    if branch.members_known && summary.members != branch.summary.members {
        return Err(corrupt(page, "membership aggregate mismatch"));
    }
    Ok(())
}
