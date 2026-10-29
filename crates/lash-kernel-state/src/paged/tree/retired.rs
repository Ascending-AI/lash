//! Non-owning transfer receipts for obsolete working views. They can restore
//! an unchanged physical representation without keeping its rows shared.
use super::*;

pub struct Retired<T: Cell> {
    sequence: bool,
    root: Option<RetiredBranch<T>>,
}
struct RetiredBranch<T: Cell> {
    family: Option<FamilyId>,
    encoded_bytes: Option<u64>,
    is_leaf: Option<bool>,
    first: Key,
    summary: Summary,
    tasks_known: bool,
    members_known: bool,
    link: RetiredLink<T>,
}
enum RetiredLink<T: Cell> {
    Sealed(PageRef),
    Inline {
        body: Inline<T>,
        children: Vec<PageRef>,
    },
    Dirty(Weak<residency::DirtyNode<T>>, u64),
}
impl<T: Cell> Tree<T> {
    /// Relinquishes strong ownership of working pages, retaining only what
    /// restores this exact view if its rows have not changed.
    pub fn retire(self) -> Retired<T> {
        Retired {
            sequence: self.sequence,
            root: self.root.map(|root| {
                let root = Rc::unwrap_or_clone(root);
                RetiredBranch {
                    family: root.family,
                    encoded_bytes: root.encoded_bytes,
                    is_leaf: root.is_leaf,
                    first: root.first,
                    summary: root.summary,
                    tasks_known: root.tasks_known,
                    members_known: root.members_known,
                    link: match root.link {
                        Link::Sealed(page) => RetiredLink::Sealed(page),
                        Link::Inline { body, children } => RetiredLink::Inline { body, children },
                        Link::Dirty(page) => {
                            RetiredLink::Dirty(Rc::downgrade(&page), page.revision.get())
                        }
                    },
                }
            }),
        }
    }
}
impl<T: Cell> Retired<T> {
    /// Restores an unchanged view. A receipt cannot recover an edited or
    /// freed page; real readers retain an owning Tree instead.
    pub fn restore(self) -> Result<Tree<T>, HeapFault> {
        Ok(Tree {
            sequence: self.sequence,
            root: self
                .root
                .map(|root| {
                    let link = match root.link {
                        RetiredLink::Sealed(page) => Link::Sealed(page),
                        RetiredLink::Inline { body, children } => Link::Inline { body, children },
                        RetiredLink::Dirty(page, revision) => {
                            let page = page
                                .upgrade()
                                .filter(|page| page.revision.get() == revision)
                                .ok_or_else(|| {
                                    HeapFault::Malformed("a retired tree view changed".into())
                                })?;
                            Link::Dirty(page)
                        }
                    };
                    Ok(Rc::new(Branch {
                        family: root.family,
                        encoded_bytes: root.encoded_bytes,
                        is_leaf: root.is_leaf,
                        first: root.first,
                        summary: root.summary,
                        tasks_known: root.tasks_known,
                        members_known: root.members_known,
                        link,
                    }))
                })
                .transpose()?,
        })
    }
}
