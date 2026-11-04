//! An actual source cursor supplies the entry of an affine call region.
use super::*;

#[derive(Clone, Copy)]
pub(crate) struct CallEntry<'a> {
    frontier: &'a Frontier,
    unpaid: &'a Point,
}

pub(in crate::compile::room) struct ScopeEntry {
    identity: std::rc::Rc<()>,
    revision: u64,
    link: lash_kernel_value::LinkId,
    position: Position,
}

impl Frontier {
    pub(crate) fn call_entry(&self) -> Option<CallEntry<'_>> {
        (!self.proven).then_some(CallEntry {
            frontier: self,
            unpaid: self.position.unpaid.as_ref()?,
        })
    }
}

impl CallEntry<'_> {
    pub(in crate::compile::room) fn bind(
        self,
        exe: &crate::compile::Executable,
        source: &Arc<Source>,
        caller: &Invocation,
    ) -> Option<ScopeEntry> {
        let frontier = self.frontier;
        let document = exe.instances.get(&frontier.link)?;
        if frontier.proven
            || !Arc::ptr_eq(source, &frontier.source)
            || frontier.position.unpaid.as_ref() != Some(self.unpaid)
            || caller.id() != frontier.position.occurrence.invocation
            || caller.frame()
                != (logical::Owner::Frame {
                    task: frontier.position.occurrence.task,
                    invocation: caller.id(),
                })
            || doc::KernelVersion::of(document.kernel()) != Some(frontier.kernel)
            || !match source.owner {
                doc::constant_pool::PoolOwner::Document(id) => document.identity() == id,
                doc::constant_pool::PoolOwner::Function(function) => {
                    exe.admits_callable(&lash_kernel_value::CallableId {
                        link: frontier.link,
                        function,
                    })
                }
            }
        {
            return None;
        }
        Some(ScopeEntry {
            identity: std::rc::Rc::clone(&frontier.identity),
            revision: frontier.revision,
            link: frontier.link,
            position: frontier.position.clone(),
        })
    }
}

impl ScopeEntry {
    pub(in crate::compile::room) fn current(&self, frontier: &Frontier) -> bool {
        !frontier.proven
            && std::rc::Rc::ptr_eq(&self.identity, &frontier.identity)
            && self.revision == frontier.revision
            && self.link == frontier.link
            && self.position == frontier.position
    }
    pub(in crate::compile::room) fn accepts(&self, entry: CallEntry<'_>) -> bool {
        self.current(entry.frontier) && self.position.unpaid.as_ref() == Some(entry.unpaid)
    }
}
