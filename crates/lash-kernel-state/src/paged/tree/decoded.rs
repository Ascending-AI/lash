//! Decoded cells retain bounded normal-form caches across repeated reads.
use super::super::{NameRef, SiteRef};
use super::*;
use lash_vocabulary::names::Name;
use lash_vocabulary::sites::Site;
use std::any::{Any, TypeId};
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Location {
    Page(PageId),
    Inline(usize),
}
type CacheKey = (Location, TypeId, Option<FamilyId>, bool);
struct Decoded {
    node: Box<dyn Any>,
    summary: Summary,
    encoded: usize,
    _inline: Option<Arc<[u8]>>,
}
#[derive(Default)]
pub(super) struct DecodedResidency {
    nodes: BTreeMap<CacheKey, Decoded>,
    pub(super) bytes: usize,
    names: BTreeMap<(usize, usize), (Name, NameRef)>,
    sites: BTreeMap<Site, SiteRef>,
    projected_names: BTreeMap<(PageId, u64, u64, u64), Name>,
}
impl Access<'_> {
    pub(super) fn decoded_node<T: Cell>(
        &self,
        page: PageRef,
        branch: &Branch<T>,
        sequence: bool,
    ) -> Result<Option<Rc<Node<T>>>, HeapFault> {
        let cache = self.decoded.borrow();
        let Some(entry) = cache.nodes.get(&(
            Location::Page(page.id),
            TypeId::of::<T>(),
            branch.family,
            sequence,
        )) else {
            return Ok(None);
        };
        let Some(node) = entry.node.downcast_ref::<Rc<Node<T>>>() else {
            return Ok(None);
        };
        if branch
            .encoded_bytes
            .is_some_and(|bytes| bytes != entry.encoded as u64)
        {
            return Err(corrupt(page, "tree root byte count mismatch"));
        }
        if branch
            .is_leaf
            .is_some_and(|leaf| leaf != matches!(&**node, Node::Leaf(_)))
        {
            return Err(corrupt(page, "tree root kind mismatch"));
        }
        validate_summary(branch, entry.summary, page)?;
        Ok(Some(Rc::clone(node)))
    }
    pub(super) fn retain_decoded<T: Cell>(
        &self,
        page: PageRef,
        family: Option<FamilyId>,
        sequence: bool,
        node: &Rc<Node<T>>,
        summary: Summary,
        encoded: usize,
    ) {
        let weight = decoded_weight(node, encoded);
        if *self.bytes.borrow() + self.decoded.borrow().bytes + weight > 64 << 20 {
            self.clear();
        }
        let mut cache = self.decoded.borrow_mut();
        cache.bytes += weight;
        cache.nodes.insert(
            (Location::Page(page.id), TypeId::of::<T>(), family, sequence),
            Decoded {
                node: Box::new(Rc::clone(node)),
                summary,
                encoded,
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
    ) -> Result<Rc<Node<T>>, HeapFault> {
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
            if let Some(node) = entry.node.downcast_ref::<Rc<Node<T>>>() {
                return Ok(Rc::clone(node));
            }
        }
        let node = Rc::new(Tree::inline_node(branch, tokens, children, true)?);
        let weight = decoded_weight(&node, tokens.len()) + tokens.len();
        if *self.bytes.borrow() + self.decoded.borrow().bytes + weight > 64 << 20 {
            self.clear();
        }
        let mut cache = self.decoded.borrow_mut();
        cache.bytes += weight;
        cache.nodes.insert(
            key,
            Decoded {
                node: Box::new(Rc::clone(&node)),
                summary: branch.summary,
                encoded: tokens.len(),
                _inline: Some(tokens.clone()),
            },
        );
        Ok(node)
    }
    pub(crate) fn metadata_name(&self, name: &Name) -> Option<NameRef> {
        self.decoded
            .borrow()
            .names
            .get(&(name.as_str().as_ptr() as usize, name.as_str().len()))
            .map(|(_, value)| value.clone())
    }
    pub(crate) fn retain_metadata_name(&self, name: &Name, value: &NameRef) {
        let weight = name.as_str().len()
            + encode_body(value).len() * 16
            + std::mem::size_of::<NameRef>()
            + 512;
        if weight > 64 << 20 {
            return;
        }
        if *self.bytes.borrow() + self.decoded.borrow().bytes + weight > 64 << 20 {
            self.clear();
        }
        let mut cache = self.decoded.borrow_mut();
        cache.bytes += weight;
        // Retain the input Arc so pointer reuse cannot alias a later name.
        cache.names.insert(
            (name.as_str().as_ptr() as usize, name.as_str().len()),
            (name.clone(), value.clone()),
        );
    }
    pub(crate) fn metadata_site(&self, site: &Site) -> Option<SiteRef> {
        self.decoded.borrow().sites.get(site).cloned()
    }
    /// Retains a self-contained site reference, so a frame or binding that
    /// names a site again does not build its path tree again (FIG-6318).
    pub(crate) fn retain_metadata_site(&self, site: &Site, value: &SiteRef) {
        let weight = encode_body(site).len() * 2
            + encode_body(value).len() * 16
            + std::mem::size_of::<SiteRef>()
            + 512;
        if *self.bytes.borrow() + self.decoded.borrow().bytes + weight > 64 << 20 {
            self.clear();
        }
        let mut cache = self.decoded.borrow_mut();
        cache.bytes += weight;
        cache.sites.insert(site.clone(), value.clone());
    }
    pub(crate) fn projected_name(
        &self,
        page: PageRef,
        summary: lash_kernel_doc::heap::TextSummary,
    ) -> Option<Name> {
        self.decoded
            .borrow()
            .projected_names
            .get(&(
                page.id,
                summary.utf8_bytes,
                summary.chars,
                summary.utf16_units,
            ))
            .cloned()
    }
    pub(crate) fn retain_projected_name(
        &self,
        page: PageRef,
        summary: lash_kernel_doc::heap::TextSummary,
        name: &Name,
    ) {
        let weight = name.as_str().len() + std::mem::size_of::<Name>() + 128;
        if weight > 64 << 20 {
            return;
        }
        if *self.bytes.borrow() + self.decoded.borrow().bytes + weight > 64 << 20 {
            self.clear();
        }
        let mut cache = self.decoded.borrow_mut();
        cache.bytes += weight;
        cache.projected_names.insert(
            (
                page.id,
                summary.utf8_bytes,
                summary.chars,
                summary.utf16_units,
            ),
            name.clone(),
        );
    }
}
/// Conservative physical allowance for cell allocations and their bounded
/// runtime caches. Nested trees retain links; their pages account separately.
pub(super) fn node_weight<T: Cell>(node: &Node<T>) -> usize {
    match node {
        Node::Leaf(rows) => leaf_weight(rows, row_sums(rows)),
        Node::Index(rows) => {
            std::mem::size_of::<Node<T>>()
                + 128
                + rows.capacity() * std::mem::size_of::<Branch<T>>()
                + rows.iter().map(|r| r.first.0.capacity()).sum::<usize>()
        }
        Node::Staged(..) => std::mem::size_of::<Node<T>>() + 128,
    }
}
/// A leaf's allowance from its rows' sums, so one changed row updates it
/// without encoding the others.
pub(super) fn leaf_weight<T: Cell>(rows: &Vec<(Key, T)>, sums: RowSums) -> usize {
    std::mem::size_of::<Node<T>>()
        + 128
        + rows.capacity() * std::mem::size_of::<(Key, T)>()
        + sums.weight
}
/// One leaf row's share of its leaf's allowance and of its encoded body.
#[derive(Clone, Copy)]
pub(super) struct RowSize {
    weight: usize,
    encoded: usize,
}
impl RowSize {
    pub(super) fn of(key: &Key, encoded: usize) -> Self {
        Self {
            weight: key.0.capacity() + encoded * 16 + 512,
            encoded: key.len() + encoded + 8,
        }
    }
}
/// The sums of a leaf's row sizes.
#[derive(Clone, Copy, Default)]
pub(super) struct RowSums {
    weight: usize,
    pub(super) encoded: usize,
}
impl RowSums {
    pub(super) fn add(mut self, row: RowSize) -> Self {
        self.weight += row.weight;
        self.encoded += row.encoded;
        self
    }
    pub(super) fn without(self, row: RowSize) -> Option<Self> {
        Some(Self {
            weight: self.weight.checked_sub(row.weight)?,
            encoded: self.encoded.checked_sub(row.encoded)?,
        })
    }
}
pub(super) fn row_sums<T: Cell>(rows: &[(Key, T)]) -> RowSums {
    let mut out = TokenWriter::new();
    rows.iter().fold(RowSums::default(), |sums, (k, v)| {
        out.clear();
        v.encode(&mut out);
        sums.add(RowSize::of(k, out.len()))
    })
}
/// [`node_weight`] of a node decoded from `encoded` token bytes, which
/// bound its rows' encodings: a read retains it without encoding each row
/// again (FIG-6321).
fn decoded_weight<T: Cell>(node: &Node<T>, encoded: usize) -> usize {
    match node {
        Node::Leaf(rows) => {
            std::mem::size_of::<Node<T>>()
                + 128
                + rows.capacity() * std::mem::size_of::<(Key, T)>()
                + rows
                    .iter()
                    .map(|(k, _)| k.0.capacity() + 512)
                    .sum::<usize>()
                + encoded * 16
        }
        Node::Index(_) | Node::Staged(..) => node_weight(node),
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
