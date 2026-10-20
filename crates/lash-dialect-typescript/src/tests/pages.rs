#![allow(
    clippy::unwrap_used,
    reason = "scripted conformance fixture construction uses admitted pages; a refusal is a defect in the fixture"
)]
//! Test-owned page storage; execution and accounting are the real S4/S7 implementations.
use lash_kernel_doc::heap::*;
use lash_kernel_doc::{Datum, Name};
use lash_kernel_state::paged::*;
use lash_kernel_vm::SessionEditor;
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};
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
#[derive(Clone, Default)]
pub(crate) struct Session {
    pub(crate) root: Option<SessionRoot>,
    pub(crate) source: Source,
}
impl Session {
    pub(crate) fn names(&self) -> BTreeSet<Name> {
        SessionEditor::open(self.root.as_ref(), Box::new(self.source.clone()))
            .unwrap()
            .names()
            .unwrap()
    }
    pub(crate) fn seeded(values: impl IntoIterator<Item = (Name, Datum)>) -> Self {
        let source = Source::default();
        let mut editor = SessionEditor::open(None, Box::new(source.clone())).unwrap();
        for (name, datum) in values {
            editor.set(&name, &datum).unwrap();
        }
        Self {
            root: Some(editor.seal().unwrap()),
            source,
        }
    }
}

pub(crate) fn start(
    program: lash_kernel_vm::Program,
    bounds: lash_kernel_vm::Bounds,
    start: lash_kernel_vm::CellStart,
) -> Result<lash_kernel_vm::KernelMachine, lash_kernel_vm::StartError> {
    with_source(program, bounds, start, &Source::default())
}
pub(crate) fn with_source(
    program: lash_kernel_vm::Program,
    bounds: lash_kernel_vm::Bounds,
    start: lash_kernel_vm::CellStart,
    source: &Source,
) -> Result<lash_kernel_vm::KernelMachine, lash_kernel_vm::StartError> {
    use lash_kernel_vm::Machine;
    lash_kernel_vm::KernelMachine::start(program, bounds, start, Box::new(source.clone()))
}
/// Seals `machine` and opens it again from its pages, as a cold worker
/// would.
pub(crate) fn reopen(
    machine: &mut lash_kernel_vm::KernelMachine,
    program: &lash_kernel_vm::Program,
    bounds: lash_kernel_vm::Bounds,
    source: &Source,
) {
    use lash_kernel_vm::Machine;
    let sealed = machine.checkpoint().unwrap();
    *machine = lash_kernel_vm::KernelMachine::open(
        program.clone(),
        bounds,
        &sealed.checkpoint(),
        Box::new(source.clone()),
    )
    .unwrap();
}
