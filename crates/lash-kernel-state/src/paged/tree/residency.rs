//! Dirty pages share a bounded staging registry across all activation trees.
use super::*;
use std::collections::VecDeque;
use std::rc::Weak;

pub(super) const DIRTY_LIMIT: usize = 32 << 20;
const DIRTY_TARGET: usize = 24 << 20;
trait Stage {
    fn spill(&self, access: &Access<'_>, sequence: bool) -> Result<(), HeapFault>;
}
impl<T: Cell> Stage for RefCell<Node<T>> {
    fn spill(&self, access: &Access<'_>, sequence: bool) -> Result<(), HeapFault> {
        let node = self.borrow().clone();
        if matches!(node, Node::Staged(..)) {
            return Ok(());
        }
        // Preparing and sealing may change storage links, never guest content.
        // All working versions sharing this node see the same immutable stage.
        let mut branch = dirty(node, access, sequence)?;
        let mut tree = Tree {
            root: Some(branch.clone()),
            sequence,
        };
        let mut work = SealWork::new(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        });
        tree.seal(access, &mut work)?;
        branch = tree.root.expect("nonempty staged tree");
        let Link::Sealed(page) = branch.link else {
            unreachable!()
        };
        *self.borrow_mut() = Node::Staged(
            page,
            branch.encoded_bytes.expect("sealed bytes"),
            branch.is_leaf.expect("sealed kind"),
        );
        Ok(())
    }
}
struct Entry {
    node: Weak<dyn Stage>,
    bytes: usize,
    sequence: bool,
}
#[derive(Default)]
pub(super) struct DirtyResidency {
    entries: BTreeMap<usize, Entry>,
    order: VecDeque<usize>,
    bytes: usize,
    staging: bool,
}
impl Access<'_> {
    pub(super) fn track<T: Cell>(
        &self,
        page: &Rc<RefCell<Node<T>>>,
        node: &Node<T>,
        sequence: bool,
    ) {
        if matches!(node, Node::Staged(..)) {
            return;
        }
        let bytes = super::decoded::node_weight(node) + std::mem::size_of::<Entry>() + 64;
        let id = Rc::as_ptr(page) as usize;
        let mut dirty = self.dirty.borrow_mut();
        // Weak references do not change copy-on-write's ownership test.
        let erased: Rc<dyn Stage> = page.clone();
        let entry = Entry {
            node: Rc::downgrade(&erased),
            bytes,
            sequence,
        };
        if let Some(old) = dirty.entries.insert(id, entry) {
            dirty.bytes -= old.bytes;
        } else {
            dirty.order.push_back(id);
        }
        dirty.bytes += bytes;
    }
    pub(super) fn stage_pressure(&self) -> Result<(), HeapFault> {
        {
            let mut dirty = self.dirty.borrow_mut();
            if dirty.staging || dirty.bytes <= DIRTY_LIMIT {
                return Ok(());
            }
            dirty.staging = true;
        }
        let result = (|| {
            loop {
                let entry = {
                    let mut dirty = self.dirty.borrow_mut();
                    if dirty.bytes <= DIRTY_TARGET {
                        break;
                    }
                    let Some(id) = dirty.order.pop_front() else {
                        break;
                    };
                    let Some(entry) = dirty.entries.remove(&id) else {
                        continue;
                    };
                    dirty.bytes -= entry.bytes;
                    (id, entry)
                };
                if let Some(node) = entry.1.node.upgrade()
                    && let Err(fault) = node.spill(self, entry.1.sequence)
                {
                    let mut dirty = self.dirty.borrow_mut();
                    dirty.bytes += entry.1.bytes;
                    dirty.order.push_front(entry.0);
                    dirty.entries.insert(entry.0, entry.1);
                    return Err(fault);
                }
            }
            Ok(())
        })();
        self.dirty.borrow_mut().staging = false;
        result
    }
    #[cfg(test)]
    pub(crate) fn dirty_bytes(&self) -> usize {
        self.dirty.borrow().bytes
    }
}
