//! Parked pages prepared privately, then handed to the same owner commit
//! as their new engine state. A reader lease owns every await and blocking
//! traversal; dropping a preparation releases its lease and private pages.
use super::{KernelProcessEngine, state::KernelEngineState};
use lash_core_store::checkpoint_graph::{EncodedNode, NodeId};
use lash_durable::domain::pages::{LeaseFence, PageStore, RootLease, RootLeaseRequest};
use lash_durable::domain::{ExecKey, RootOwner};
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_migrate::{CarryFault, ParkedRefusal, Rewritten};
use lash_kernel_state::paged::{Checkpoint, PageAnswer, PageBody, PageSource};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

struct Lease {
    store: Arc<dyn PageStore>,
    lease: RootLease,
    runtime: tokio::runtime::Handle,
    _release: ReleaseLease,
}
struct ReleaseLease(Option<Box<dyn FnOnce() + Send + Sync>>);
impl Drop for ReleaseLease {
    fn drop(&mut self) {
        if let Some(release) = self.0.take() {
            release();
        }
    }
}
#[derive(Clone)]
struct Pages {
    lease: Arc<Lease>,
    nodes: Arc<Mutex<BTreeMap<NodeId, EncodedNode>>>,
}
impl Pages {
    fn nodes(&self) -> std::sync::MutexGuard<'_, BTreeMap<NodeId, EncodedNode>> {
        self.nodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        if pages.len() > lash_kernel_state::paged::PagePortLimits::STANDARD.pages {
            return Err(HeapFault::Exhausted {
                resource: lash_kernel_doc::heap::HostResource::ReadBatch,
            });
        }
        pages
            .iter()
            .map(|page| {
                let id = NodeId(page.id.0);
                let bytes = self.nodes().get(&id).map(|node| Arc::clone(&node.bytes));
                let bytes = match bytes {
                    Some(bytes) => bytes,
                    None => {
                        let read = self
                            .lease
                            .runtime
                            .block_on(self.lease.store.read_pages(&self.lease.lease, &[id]))
                            .map_err(|error| HeapFault::Transport(error.to_string()))?;
                        let stored = read
                            .into_iter()
                            .next()
                            .ok_or(HeapFault::Missing { page: page.id })?;
                        if stored.id != id {
                            return Err(HeapFault::Corrupt {
                                page: page.id,
                                reason: "another page answered".into(),
                            });
                        }
                        stored.bytes
                    }
                };
                Ok(lash_vm_broker::kernel::pages::answer_page(
                    *page,
                    &bytes,
                    |_| Ok(PageGrant::NONE),
                ))
            })
            .collect()
    }
    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault> {
        let node = lash_vm_broker::kernel::pages::frame_page(&page)?.named();
        let reference = PageRef {
            id: PageId(node.id.0),
            grant: PageGrant::NONE,
        };
        self.nodes().insert(node.id, node.into_node());
        Ok(reference)
    }
}
fn refused(detail: impl Into<String>) -> CarryFault {
    ParkedRefusal::StateNotCarried {
        detail: detail.into(),
    }
    .into()
}

/// A carry cold-loads its exact pinned Link DAG; there is no package-set fallback.
pub async fn carry_snapshot(
    documents: &super::KernelDocuments,
    link: lash_vocabulary::package::LinkId,
) -> Result<lash_kernel_package::LinkedContent, CarryFault> {
    documents
        .load_linked(link)
        .await
        .map_err(|error| refused(error.to_string()))
}

impl KernelProcessEngine {
    /// Prepares pages and a new requirements record without publishing any
    /// run metadata. `staged` is transferred to the fenced owner transaction.
    pub(super) async fn carry_pages(
        &self,
        process: &lash_core::ProcessId,
        state: &mut KernelEngineState,
        rewritten: &Rewritten,
        successor: Option<&lash_vocabulary::names::LinkedStart>,
        redeclared: Option<&lash_kernel_doc::FunctionRegistry>,
    ) -> Result<Vec<EncodedNode>, CarryFault> {
        let Some(parked) = state.parked else {
            return Ok(Vec::new());
        };
        let input = super::state::KernelProcessInput::from_payload(&state.payload)
            .map_err(|error| refused(error.to_string()))?;

        let super::InlineRequest::Linked { start } = &input.code else {
            return Err(refused(
                "captured callable carry requires a capture correspondence",
            ));
        };
        let content = carry_snapshot(&self.documents, start.link).await?;
        let pinned_document = content
            .document(
                content
                    .link(start.link)
                    .map_err(|e| refused(e.to_string()))?
                    .document,
            )
            .map_err(|e| refused(e.to_string()))?;
        let stored: BTreeMap<_, _> = content
            .documents
            .values()
            .flat_map(|document| {
                document
                    .functions
                    .iter()
                    .map(|(id, f)| (*id, Arc::new(f.clone())))
            })
            .collect();
        let old_closure = lash_vm_broker::kernel::closure::derive(
            pinned_document.functions.keys().copied(),
            &self.functions,
            &stored,
        )
        .map_err(|e| refused(e.to_string()))?;
        let next = successor.unwrap_or(start);
        let next_content = carry_snapshot(&self.documents, next.link).await?;
        let selected = next_content
            .inspect(
                next.link,
                &lash_kernel_package::ReachSelection::Start(next.selector.clone()),
            )
            .map_err(|e| refused(e.to_string()))?;
        let selected_ids: std::collections::BTreeSet<_> =
            selected.functions.values().flatten().copied().collect();
        let stored: BTreeMap<_, _> = next_content
            .documents
            .values()
            .flat_map(|d| &d.functions)
            .filter(|(id, _)| selected_ids.contains(id))
            .map(|(id, f)| (*id, Arc::new(f.clone())))
            .collect();
        let held = redeclared.unwrap_or(&self.functions);
        let declared = lash_vm_broker::kernel::loader::declare(
            selected_ids.iter().copied(),
            rewritten.document.kernel(),
            held,
            &stored,
        )
        .map_err(|e| refused(e.to_string()))?;
        let closure = lash_vm_broker::kernel::closure::derive(selected_ids, held, &stored)
            .map_err(|e| refused(e.to_string()))?;
        let stores = self
            .carry_stores
            .as_ref()
            .ok_or_else(|| refused("this engine has no page store for carry"))?;
        let durable = stores.durable_store();
        let pages = durable
            .pages()
            .ok_or_else(|| refused("the store has no page port"))?;
        let exec = ExecKey::Process(process.clone());
        let row = pages
            .exec_root(&exec)
            .await
            .map_err(|error| HeapFault::Transport(error.to_string()))?
            .ok_or_else(|| refused("the process has no execution root"))?;
        let kernel = rewritten.document.kernel();
        let correspondence = rewritten.correspondence.clone();
        let program = lash_kernel_vm::Program::prepare(
            next.link,
            next_content
                .documents
                .iter()
                .map(|(id, d)| (*id, Arc::new(d.clone())))
                .collect(),
            &next_content.links,
            PageId(closure.reference().0),
            lash_kernel_vm::PreparedLibrary::new(Arc::new(declared)),
        )
        .map_err(|e| refused(e.to_string()))?;
        let (page, mut nodes) = carry_parked_checkpoint(
            pages,
            RootLeaseRequest {
                owner: RootOwner::Exec(exec.clone()),
                expected: row.root,
                fence: LeaseFence::Reader,
                ttl_millis: 600_000,
            },
            parked.page.root,
            rewritten.clone(),
            program,
            self.bounds,
            (pinned_document.kernel(), PageId(old_closure.reference().0)),
        )
        .await?;
        nodes.extend(closure.nodes().values().cloned());
        if let Some(parked) = &mut state.parked {
            parked.page = super::state::ParkedPage { root: page, kernel };
        }
        // Only code requirements change here. The owner derives link requirements
        // from the successor env at the same commit that repoints that env.
        state.requirements.machine = selected.machine;
        for held in state.admissions.values_mut() {
            held.admission.0.identity =
                lash_kernel_migrate::carry_effect(&correspondence, &held.admission.0.identity)?;
        }
        for wait in state.waits.values_mut() {
            let (super::state::Wait::Step { identity, .. }
            | super::state::Wait::Timer { identity, .. }) = wait;
            *identity = lash_kernel_migrate::carry_effect(&correspondence, identity)?;
        }
        for delivery in &mut state.settled {
            delivery.identity =
                lash_kernel_migrate::carry_effect(&correspondence, &delivery.identity)?;
        }
        Ok(nodes)
    }
}
/// Prepares a foreign checkpoint under a lease. Publishing the returned nodes
/// and repointing the execution root remain one fenced owner transaction.
pub async fn carry_parked_checkpoint(
    store: Arc<dyn PageStore>,
    request: RootLeaseRequest,
    root: NodeId,
    rewritten: Rewritten,
    program: lash_kernel_vm::Program,
    bounds: lash_kernel_vm::Bounds,
    pinned: (u32, PageId),
) -> Result<(NodeId, Vec<EncodedNode>), CarryFault> {
    let RootOwner::Exec(exec) = &request.owner else {
        return Err(refused("a checkpoint carry requires an execution owner"));
    };
    let owner = lash_vm_broker::kernel::graph::owner_binding_node(exec)
        .map_err(|error| refused(error.to_string()))?;
    let lease = store
        .acquire_root_lease(request)
        .await
        .map_err(|error| HeapFault::Transport(error.to_string()))?;
    let runtime = tokio::runtime::Handle::current();
    let release_store = Arc::clone(&store);
    let release_runtime = runtime.clone();
    let id = lease.id;
    let release = ReleaseLease(Some(Box::new(move || {
        release_runtime.spawn(async move {
            let _ = release_store.release_root_lease(id).await;
        });
    })));
    let pages = Pages {
        lease: Arc::new(Lease {
            store,
            lease,
            runtime,
            _release: release,
        }),
        nodes: Arc::default(),
    };
    tokio::task::spawn_blocking(move || {
        let mut source = pages;
        let reference = PageRef {
            id: PageId(root.0),
            grant: PageGrant::NONE,
        };
        let answer = source.read(&[reference])?.remove(0)?;
        let checkpoint = Checkpoint::of_answer(&answer)?;
        if lash_kernel_state::kernel_internals::checkpoint_root(&checkpoint).link != program.link {
            return Err(refused(
                "checkpoint Link carry requires explicit callable and frame correspondence",
            ));
        }
        if checkpoint.header().owner != PageId(owner.0) {
            return Err(refused("checkpoint belongs to another execution owner"));
        }
        let (page, checkpoint) = lash_kernel_migrate::carry_checkpoint(
            &mut source,
            &checkpoint,
            &rewritten,
            program.closure,
            pinned,
        )?;
        lash_kernel_vm::validate_foreign_checkpoint(
            program,
            bounds,
            &checkpoint,
            Box::new(source.clone()),
        )
        .map_err(|error| match error {
            lash_kernel_vm::OpenError::Heap(HeapFault::Malformed(problem)) => {
                refused(format!("foreign continuation: {problem}"))
            }
            lash_kernel_vm::OpenError::Heap(fault) => CarryFault::Heap(fault),
            other => refused(format!("foreign image: {other}")),
        })?;
        let nodes = source.nodes().values().cloned().collect();
        Ok((NodeId(page.id.0), nodes))
    })
    .await
    .map_err(|error| HeapFault::Transport(error.to_string()))?
}
