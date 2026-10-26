use lash_kernel_doc::heap::{HeapFault, HostResource, PageGrant, PageId, PageRef};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

#[derive(Clone)]
pub(crate) struct Pages(Rc<RefCell<Store>>);
struct Store {
    pages: BTreeMap<PageId, PageAnswer>,
    bytes: usize,
    limit: usize,
}
impl Pages {
    pub fn new(limit: usize) -> Self {
        Self(Rc::new(RefCell::new(Store {
            pages: BTreeMap::new(),
            bytes: 0,
            limit,
        })))
    }
}
impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        let store = self.0.borrow();
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
        let mut tokens = body.kind.0.to_be_bytes().to_vec();
        tokens.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(lash_kernel_doc::heap_values::key_digest(&tokens)),
            grant: PageGrant::NONE,
        };
        let mut store = self.0.borrow_mut();
        let Store {
            pages,
            bytes: held,
            limit,
        } = &mut *store;
        if let std::collections::btree_map::Entry::Vacant(entry) = pages.entry(page.id) {
            let bytes = body
                .tokens
                .len()
                .saturating_add(
                    body.children
                        .len()
                        .saturating_mul(std::mem::size_of::<PageRef>()),
                )
                .saturating_add(std::mem::size_of::<PageAnswer>());
            let total =
                held.checked_add(bytes)
                    .filter(|n| *n <= *limit)
                    .ok_or(HeapFault::Exhausted {
                        resource: HostResource::StagingBytes,
                    })?;
            entry.insert(PageAnswer {
                page,
                kind: body.kind,
                tokens: body.tokens.into(),
                children: body.children,
            });
            *held = total;
        }
        Ok(page)
    }
}
