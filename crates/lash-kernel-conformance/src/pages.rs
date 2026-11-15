#![allow(
    clippy::unwrap_used,
    reason = "scripted conformance fixture construction uses admitted pages; a refusal is a defect in the fixture"
)]
//! Test-owned page storage; execution and accounting are the real S4/S7 implementations.
use lash_kernel_doc::Datum;
use lash_kernel_doc::heap::*;
use lash_kernel_state::paged::*;
use lash_kernel_value::names::Name;
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};
#[derive(Clone, Default)]
pub(crate) struct Source {
    pages: Rc<RefCell<BTreeMap<PageId, PageAnswer>>>,
}
impl PageSource for Source {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|p| {
                self.pages
                    .borrow()
                    .get(&p.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: p.id })
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let mut tokens = body.kind.0.to_be_bytes().to_vec();
        tokens.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(lash_kernel_doc::heap_values::key_digest(&tokens)),
            grant: PageGrant::NONE,
        };
        self.pages.borrow_mut().insert(
            page.id,
            PageAnswer {
                page,
                kind: body.kind,
                tokens: body.tokens.into(),
                children: body.children,
            },
        );
        Ok(page)
    }
}

pub(crate) const CLOSURE: PageId = PageId([7; 32]);
pub(crate) const OWNER: PageId = PageId([9; 32]);
pub(crate) const BUDGET: SealBudget = SealBudget {
    pages: u64::MAX,
    bytes: u64::MAX,
};
#[derive(Clone, Default)]
pub(crate) struct Session {
    pub(crate) root: Option<SessionRoot>,
    pub(crate) source: Source,
}
impl Session {
    #[cfg(test)]
    pub(crate) fn names(&self) -> std::collections::BTreeSet<Name> {
        lash_kernel_vm::SessionEditor::open(self.root.as_ref(), Box::new(self.source.clone()))
            .unwrap()
            .names()
            .unwrap()
    }
    pub(crate) fn seeded(values: impl IntoIterator<Item = (Name, Datum)>) -> Self {
        let source = Source::default();
        let mut editor =
            lash_kernel_vm::SessionEditor::open(None, Box::new(source.clone())).unwrap();
        for (name, datum) in values {
            editor.set(&name, &datum).unwrap();
        }
        Self {
            root: Some(editor.seal().unwrap()),
            source,
        }
    }
}

#[cfg(test)]
pub(crate) fn start(
    program: lash_kernel_vm::Program,
    bounds: lash_kernel_vm::Bounds,
    start: lash_kernel_vm::CellStart,
) -> Result<lash_kernel_vm::KernelMachine, lash_kernel_vm::StartError> {
    with_source(program, bounds, start, &Source::default())
}
#[cfg(test)]
pub(crate) fn with_source(
    program: lash_kernel_vm::Program,
    bounds: lash_kernel_vm::Bounds,
    start: lash_kernel_vm::CellStart,
    source: &Source,
) -> Result<lash_kernel_vm::KernelMachine, lash_kernel_vm::StartError> {
    <lash_kernel_vm::KernelMachine as lash_kernel_vm::Machine>::start(
        program,
        bounds,
        start,
        Box::new(source.clone()),
    )
}
pub(crate) fn open<M: lash_kernel_vm::Machine>(
    program: lash_kernel_vm::Program,
    bounds: lash_kernel_vm::Bounds,
    root: &CheckpointRoot,
    source: &Source,
) -> Result<M, lash_kernel_vm::OpenError> {
    M::open(
        program,
        bounds,
        &lash_kernel_state::kernel_internals::checkpoint(root.clone()),
        Box::new(source.clone()),
    )
}
