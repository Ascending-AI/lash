//! An in-process page port that counts what laws read and seal.
use super::super::*;
use lash_kernel_doc::heap::*;
use lash_kernel_doc::heap_values::key_digest;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

#[derive(Default)]
pub(super) struct Pages {
    pub pages: BTreeMap<PageId, PageAnswer>,
    pub reads: u64,
    pub payload_reads: u64,
    pub seals: u64,
    /// Seals by page kind, re-encodings of an existing page included.
    pub kinds: BTreeMap<PageKind, u64>,
    /// Seals of a page the port already held.
    pub resealed: u64,
    pub bytes: u64,
    pub seal_fault: Option<HeapFault>,
}
pub(super) struct Source(pub Rc<RefCell<Pages>>);
impl PageSource for Source {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        let mut store = self.0.borrow_mut();
        store.reads += pages.len() as u64;
        store.payload_reads += pages
            .iter()
            .filter(|page| {
                store.pages.get(&page.id).is_some_and(|answer| {
                    matches!(
                        answer.kind,
                        PageKind::PAYLOAD_LEAF | PageKind::PAYLOAD_INDEX
                    )
                })
            })
            .count() as u64;
        Ok(pages
            .iter()
            .map(|p| {
                store
                    .pages
                    .get(&p.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: p.id })
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        if let Some(fault) = self.0.borrow().seal_fault.clone() {
            return Err(fault);
        }
        let mut tokens = body.kind.0.to_be_bytes().to_vec();
        tokens.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(key_digest(&tokens)),
            grant: PageGrant::NONE,
        };
        let mut store = self.0.borrow_mut();
        store.seals += 1;
        store.resealed += u64::from(store.pages.contains_key(&page.id));
        *store.kinds.entry(body.kind).or_default() += 1;
        store.bytes += body.tokens.len() as u64 + 32 * body.children.len() as u64;
        store.pages.insert(
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

/// An empty port and an access over it.
pub(super) fn fresh() -> (tree::Access<'static>, Rc<RefCell<Pages>>) {
    let pages = Rc::new(RefCell::new(Pages::default()));
    (
        tree::Access::new(Box::new(Source(Rc::clone(&pages)))),
        pages,
    )
}
