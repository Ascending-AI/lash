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
                let bytes = self.nodes().get(&id).map(|node| node.bytes.clone());
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

/// Reads exactly the pinned snapshot and checks that it supplies each pinned
/// declaration. Another retained set is never a fallback for missing code.
pub async fn carry_snapshot(
    authority: &dyn crate::InternalAuthority,
    held: &lash_kernel_package::HeldPackages,
    pinned: lash_kernel_doc::PackageSetDigest,
    functions: impl IntoIterator<Item = lash_kernel_doc::FunctionId>,
) -> Result<Arc<lash_kernel_package::SurfaceSnapshot>, CarryFault> {
    let surface = if pinned == held.digest() {
        Arc::clone(held.surface())
    } else {
        let bytes = authority
            .package_surface(&pinned)
            .await
            .map_err(|error| HeapFault::Transport(error.to_string()))?
            .ok_or_else(|| refused(format!("pinned snapshot {pinned} is unavailable")))?;
        Arc::new(
            lash_kernel_package::SurfaceSnapshot::decode(pinned, &bytes)
                .map_err(|error| refused(format!("pinned snapshot {pinned}: {error}")))?,
        )
    };
    let supplied = surface.functions();
    for function in functions {
        if !supplied.contains(&function) {
            return Err(refused(format!(
                "pinned snapshot {pinned} lacks function {function}"
            )));
        }
    }
    Ok(surface)
}

impl KernelProcessEngine {
    /// Prepares pages and a new requirements record without publishing any
    /// run metadata. `staged` is transferred to the fenced owner transaction.
    pub(super) async fn carry_pages(
        &self,
        process: &lash_core::ProcessId,
        state: &mut KernelEngineState,
        rewritten: &Rewritten,
        environment: Option<&lash_execution_contracts::process::ProcessEnv>,
        redeclared: Option<&lash_kernel_doc::FunctionRegistry>,
    ) -> Result<Vec<EncodedNode>, CarryFault> {
        let Some(parked) = state.parked else {
            return Ok(Vec::new());
        };
        let input = super::state::KernelProcessInput::from_payload(&state.payload)
            .map_err(|error| refused(error.to_string()))?;

        let pinned_document = self
            .documents
            .get(&rewritten.correspondence.base)
            .await
            .map_err(|error| HeapFault::Transport(error.to_string()))?
            .ok_or_else(|| refused("pinned document is missing"))?;
        let old_stored = self
            .documents
            .stored_definitions(&pinned_document, &self.functions)
            .await
            .map_err(|error| HeapFault::Transport(error.to_string()))?
            .map_err(|error| refused(format!("pinned snapshot {}: {error:?}", input.packages)))?;
        let old_closure = lash_vm_broker::kernel::closure::derive(
            pinned_document.manifest.functions.keys().copied(),
            &self.functions,
            &old_stored,
        )
        .map_err(|error| refused(error.to_string()))?;
        let supplied = if environment.is_some() {
            &rewritten.document
        } else {
            &pinned_document
        };
        carry_snapshot(
            self.authority.as_ref(),
            &self.packages,
            input.packages,
            supplied.manifest.functions.keys().copied(),
        )
        .await?;
        let needed = rewritten
            .document
            .manifest
            .functions
            .keys()
            .filter(|id| self.functions.get(id).is_none())
            .copied()
            .collect::<Vec<_>>();
        let mut stored = BTreeMap::new();
        let mut unread = Vec::new();
        for function in needed {
            if let Some(definition) = redeclared.and_then(|registry| registry.get(&function)) {
                stored.insert(function, Arc::clone(definition.definition()));
            } else {
                unread.push(function);
            }
        }
        stored.extend(
            self.documents
                .stored_functions(unread)
                .await
                .map_err(|error| HeapFault::Transport(error.to_string()))?
                .map_err(|error| {
                    refused(format!("pinned snapshot {}: {error:?}", input.packages))
                })?,
        );
        let mut declarations = stored.clone();
        for function in rewritten.document.manifest.functions.keys() {
            if let Some(held) = self.functions.get(function) {
                declarations.insert(*function, Arc::clone(held.definition()));
            }
        }
        crate::admit_internal(self.authority.as_ref(), &declarations)
            .await
            .map_err(|error| HeapFault::Transport(error.to_string()))?
            .map_err(|error| refused(error.to_string()))?;
        let declared = lash_vm_broker::kernel::loader::declare(
            rewritten.document.manifest.functions.keys().copied(),
            rewritten.document.manifest.kernel,
            &self.functions,
            &stored,
        )
        .map_err(|error| refused(error.to_string()))?;
        let closure = lash_vm_broker::kernel::closure::derive(
            rewritten.document.manifest.functions.keys().copied(),
            &self.functions,
            &stored,
        )
        .map_err(|error| refused(error.to_string()))?;
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
        let kernel = rewritten.document.manifest.kernel;
        let correspondence = rewritten.correspondence.clone();
        let program = lash_kernel_vm::Program {
            closure: PageId(closure.reference().0),
            document: Arc::new(rewritten.document.clone()),
            library: lash_kernel_vm::PreparedLibrary::new(Arc::new(declared)),
        };
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
            (
                pinned_document.manifest.kernel,
                PageId(old_closure.reference().0),
            ),
        )
        .await?;
        nodes.extend(closure.nodes().values().cloned());
        if let Some(parked) = &mut state.parked {
            parked.page = super::state::ParkedPage { root: page, kernel };
        }
        // Only code requirements change here. The owner derives link requirements
        // from the successor env at the same commit that repoints that env.
        let links = state.requirements.clone();
        state.requirements =
            lash_vm_broker::kernel::loader::requirements(&closure, kernel, std::iter::empty());
        if let Some(environment) = environment {
            for link in environment.effects().values() {
                link.require(&mut state.requirements);
            }
        } else {
            state.requirements.providers = links.providers;
            state.requirements.codecs = links.codecs;
        }
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
