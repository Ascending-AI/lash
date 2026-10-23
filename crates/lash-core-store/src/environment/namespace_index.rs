//! Segment index for package roots and member leaves. Insertion visits only
//! the path and its immediate declared members, never every prior package.
use lash_vocabulary::names::{PackagePath, Segment};
use lash_vocabulary::package::PackageAdvertisement;
use std::collections::BTreeMap;

#[derive(Default)]
struct Node {
    children: BTreeMap<Segment, Node>,
    package: Option<PackagePath>,
    blocked: Option<PackagePath>,
    descendant: Option<PackagePath>,
}
#[derive(Default)]
pub struct NamespaceIndex {
    root: Node,
    order: BTreeMap<PackagePath, usize>,
}
impl NamespaceIndex {
    /// Refuse with the previously installed package path. A refused index is
    /// discarded with its candidate; no partially accepted namespace escapes.
    pub fn insert(&mut self, advertisement: &PackageAdvertisement) -> Result<(), PackagePath> {
        let path = match advertisement {
            PackageAdvertisement::Enumerated(package) => &package.path,
            PackageAdvertisement::Deferred(package) => &package.path,
        };
        let mut node = &mut self.root;
        for segment in path.as_slice() {
            if let Some(prior) = &node.blocked {
                return Err(prior.clone());
            }
            node.descendant.get_or_insert_with(|| path.clone());
            node = node.children.entry(segment.clone()).or_default();
        }
        if let Some(prior) = node.package.as_ref().or(node.blocked.as_ref()) {
            return Err(prior.clone());
        }
        match advertisement {
            PackageAdvertisement::Deferred(_) => {
                if let Some(prior) = &node.descendant {
                    return Err(prior.clone());
                }
                node.blocked = Some(path.clone());
            }
            PackageAdvertisement::Enumerated(package) => {
                let conflict = package
                    .members
                    .keys()
                    .filter_map(|member| {
                        let child = node.children.get(member)?;
                        child
                            .package
                            .as_ref()
                            .or(child.blocked.as_ref())
                            .or(child.descendant.as_ref())
                    })
                    .min_by_key(|prior| self.order.get(*prior).copied());
                if let Some(prior) = conflict {
                    return Err(prior.clone());
                }
                for member in package.members.keys() {
                    node.children.entry(member.clone()).or_default().blocked = Some(path.clone());
                }
            }
        }
        self.order.insert(path.clone(), self.order.len());
        node.package = Some(path.clone());
        Ok(())
    }
}
