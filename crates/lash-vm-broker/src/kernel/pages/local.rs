//! The page port of an embedder with no parent: the heap reads committed
//! pages the embedder holds itself and stages what it seals in a spool.
//! There is no separate authority to check, so every grant is
//! [`PageGrant::NONE`]; bytes are still re-hashed and re-framed before the
//! heap decodes them.

use std::sync::Arc;

use lash_core_store::checkpoint_graph::NodeId;
use lash_core_store::checkpoint_graph::stage::{StageFault, StageSpool};
use lash_kernel_doc::heap::{HeapFault, HostResource, PageGrant, PageId, PageRef};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource, source::READ_BATCH_PAGES};

use super::{answer_page, frame_page};

/// Committed pages an in-process embedder holds: a store snapshot, a
/// fixture, a test's map.
pub trait CommittedPages {
    /// The envelope of `page`, if it is committed.
    fn page(&self, page: &NodeId) -> Option<Arc<[u8]>>;
}

/// An in-process [`PageSource`]: committed pages from `committed`, sealed
/// pages into and back out of `spool`.
pub struct LocalPages<C> {
    committed: C,
    spool: Box<dyn StageSpool>,
}

impl<C: CommittedPages> LocalPages<C> {
    pub fn new(committed: C, spool: Box<dyn StageSpool>) -> Self {
        Self { committed, spool }
    }

    /// The spool, for the embedder's publication.
    pub fn spool(&mut self) -> &mut dyn StageSpool {
        self.spool.as_mut()
    }
}

fn staging(fault: StageFault) -> HeapFault {
    match fault {
        StageFault::Quota { .. } => HeapFault::Exhausted {
            resource: HostResource::StagingBytes,
        },
        StageFault::Unauthorized { child, .. } => HeapFault::Unauthorized {
            page: PageId(child.0),
        },
        StageFault::Missing(page) => HeapFault::Missing {
            page: PageId(page.0),
        },
        StageFault::HashMismatch(page) => HeapFault::Corrupt {
            page: PageId(page.0),
            reason: "a staged page does not hash to its identity".to_owned(),
        },
        StageFault::Ended => HeapFault::LeaseLost,
        StageFault::Io(error) => HeapFault::Transport(error),
    }
}

impl<C: CommittedPages> PageSource for LocalPages<C> {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        if pages.len() > READ_BATCH_PAGES {
            return Err(HeapFault::Exhausted {
                resource: HostResource::ReadBatch,
            });
        }
        pages
            .iter()
            .map(|page| {
                let id = NodeId(page.id.0);
                let bytes = match self.spool.read(&id) {
                    Ok(bytes) => bytes,
                    Err(StageFault::Missing(_)) => self
                        .committed
                        .page(&id)
                        .ok_or(HeapFault::Missing { page: page.id })?,
                    Err(fault) => return Err(staging(fault)),
                };
                answer_page(*page, &bytes, |_| Ok(PageGrant::NONE))
            })
            .collect()
    }

    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault> {
        let node = frame_page(&page)?;
        let id = PageId(node.id.0);
        let committed = &self.committed;
        self.spool
            .seal(node, &|child| committed.page(child).is_some())
            .map_err(staging)?;
        Ok(PageRef {
            id,
            grant: PageGrant::NONE,
        })
    }
}
