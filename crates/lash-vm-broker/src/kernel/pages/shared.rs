//! A session turn's shared staging, and the parent's own seals (FIG-6193).
//!
//! The cells of one turn stage into one spool: a cell's carry names pages
//! an earlier cell of the turn staged, and the turn's commit publishes
//! what the last cell left. Each cell's activation is its own page service
//! over the shared spool ([`SharedSpool`]); the turn drops the spool when
//! its commit settles. An activation's page service is shared in turn
//! between the worker's exchanges and the broker's publication
//! ([`SharedPages`]).

use std::sync::{Arc, Mutex};

use lash_core_store::checkpoint_graph::node::VerifiedNode;
use lash_core_store::checkpoint_graph::stage::{
    CandidateManifest, StageFault, StageLeaseId, StageSpool, StageUsage,
};
use lash_core_store::checkpoint_graph::{CommitId, NodeId};
use lash_durable::domain::pages::PageStore;
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_vm_protocol::page::{PageReply, PageRequest};

use super::{ActivationPages, DiskSpool, PageExchange, frame_page, staging};

/// One spool shared by the activations of a session turn. Discarding an
/// activation's handle keeps the spool: the turn's last handle deletes it.
#[derive(Clone)]
pub struct SharedSpool(Arc<Mutex<DiskSpool>>);

impl SharedSpool {
    #[must_use]
    pub fn new(spool: DiskSpool) -> Self {
        Self(Arc::new(Mutex::new(spool)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DiskSpool> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl StageSpool for SharedSpool {
    fn lease(&self) -> StageLeaseId {
        self.lock().lease()
    }

    fn seal(
        &mut self,
        page: VerifiedNode,
        protected: &dyn Fn(&NodeId) -> bool,
    ) -> Result<(), StageFault> {
        self.lock().seal(page, protected)
    }

    fn read(&self, page: &NodeId) -> Result<Arc<[u8]>, StageFault> {
        self.lock().read(page)
    }

    fn contains(&self, page: &NodeId) -> bool {
        self.lock().contains(page)
    }

    fn retain(&mut self, roots: &[NodeId]) -> Result<(), StageFault> {
        self.lock().retain(roots)
    }

    fn candidate(
        &mut self,
        root: NodeId,
        base: Option<NodeId>,
        commit: CommitId,
    ) -> Result<Box<dyn CandidateManifest>, StageFault> {
        self.lock().candidate(root, base, commit)
    }

    fn settle(&mut self, commit: CommitId) -> Result<(), StageFault> {
        self.lock().settle(commit)
    }

    fn withdraw(&mut self, commit: CommitId) -> Result<(), StageFault> {
        self.lock().withdraw(commit)
    }

    fn discard(self: Box<Self>) {}

    fn usage(&self) -> StageUsage {
        self.lock().usage()
    }
}

/// One activation's page service, shared by the machine that answers its
/// worker's exchanges and the broker that publishes its candidates.
pub type SharedPages = Arc<tokio::sync::Mutex<ActivationPages<dyn PageStore>>>;

#[async_trait::async_trait]
impl PageExchange for SharedPages {
    async fn serve(&mut self, request: PageRequest) -> PageReply {
        self.lock().await.serve(request).await
    }
}

/// The parent's own seals into a spool: a session descriptor over a
/// carried session, which names pages the spool staged or the store holds.
/// It reads nothing; every child is taken as protected, and publication
/// checks each one is stored or staged.
pub struct ParentSeal<'a>(pub &'a mut dyn StageSpool);

impl PageSource for ParentSeal<'_> {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|page| Err(HeapFault::Unauthorized { page: page.id }))
            .collect())
    }

    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault> {
        let node = frame_page(&page)?.named();
        let id = PageId(node.id.0);
        self.0.seal(node, &|_| true).map_err(staging)?;
        Ok(PageRef {
            id,
            grant: PageGrant::NONE,
        })
    }
}

/// One tick of a held activation's lease schedule (D-LEASE): sleeps on the
/// activation's clock until its renewal is due, then keeps the lease
/// alive as any exchange would. Never ends for an activation with no
/// lease, or one already refused.
pub async fn renew_when_due<R: super::LeasedReads + ?Sized>(
    pages: &Arc<tokio::sync::Mutex<ActivationPages<R>>>,
) {
    let armed = {
        let pages = pages.lock().await;
        pages.renew_at()
    };
    let Some((clock, due)) = armed else {
        return std::future::pending().await;
    };
    clock.sleep_until(due).await;
    let _ = pages.lock().await.keep_alive().await;
}

/// Keeps an activation's authority alive while its machine computes without
/// page exchanges. Dropping the work drops its renewal too.
pub async fn while_held<R: super::LeasedReads + ?Sized, F: std::future::Future>(
    pages: &Arc<tokio::sync::Mutex<ActivationPages<R>>>,
    work: F,
) -> F::Output {
    tokio::pin!(work);
    loop {
        tokio::select! {
            result = &mut work => return result,
            () = renew_when_due(pages) => {}
        }
    }
}
