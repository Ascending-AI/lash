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
pub(super) struct Pages {
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

/// A carry cold-loads its exact pinned Link DAG.
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
        retained: &std::collections::BTreeSet<lash_vocabulary::CallableId>,
        redeclared: Option<&lash_kernel_doc::FunctionRegistry>,
    ) -> Result<
        (
            Vec<EncodedNode>,
            std::collections::BTreeSet<lash_vocabulary::CallableId>,
        ),
        CarryFault,
    > {
        let Some(parked) = state.parked else {
            return Ok(Default::default());
        };
        let input = super::state::KernelProcessInput::from_payload(&state.payload)
            .map_err(|error| refused(error.to_string()))?;

        let super::InlineRequest::Linked { start } = &input.code else {
            return Err(refused(
                "captured callable carry requires a capture correspondence",
            ));
        };
        let prior_content = self
            .documents
            .load_execution_content(&input.code, retained)
            .await
            .map_err(|error| refused(error.to_string()))?;
        let content = &prior_content.content;
        let stores = self
            .carry_stores
            .as_ref()
            .ok_or_else(|| refused("this engine has no carry stores"))?;
        let pages = stores
            .durable_store()
            .pages()
            .ok_or_else(|| refused("this engine has no page store"))?;
        let exec = ExecKey::Process(process.clone());
        let row = pages
            .exec_root(&exec)
            .await
            .map_err(|error| HeapFault::Transport(error.to_string()))?
            .ok_or_else(|| refused("the process has no execution root"))?;
        let binding = lash_vm_broker::kernel::graph::owner_binding_node(&exec)
            .map_err(|error| refused(error.to_string()))?;
        let restore_selection = super::pages::check_parked_helpers(
            Arc::clone(&pages),
            RootLeaseRequest { owner: RootOwner::Exec(exec.clone()), expected: row.root, fence: LeaseFence::Reader, ttl_millis: 600_000 },
            parked.page.root, binding, content, Arc::clone(&self.helpers),
        ).await.map_err(|error| match error {
            lash_vm_broker::kernel::KernelFailure::Worker {
                outcome: lash_protocol_types::engine_fault::InfrastructureOutcome::RunRefused {
                    refusal: lash_protocol_types::engine_fault::RunRefusal::Restore {
                        refusal: lash_protocol_types::engine_fault::KernelRestoreRefusal::MissingHelper { function },
                    },
                },
            } => CarryFault::Refused(ParkedRefusal::MissingHelper { function }),
            other => refused(other.to_string()),
        })?;
        let prior = self
            .documents
            .admit_execution(prior_content, &restore_selection)
            .map_err(|error| refused(error.to_string()))?;
        let pinned_document = prior
            .content
            .document(
                prior
                    .content
                    .link(start.link)
                    .map_err(|error| refused(error.to_string()))?
                    .document,
            )
            .map_err(|error| refused(error.to_string()))?;
        let old = lash_vm_broker::kernel::loader::declare_reach(
            pinned_document,
            &prior
                .content
                .documents
                .iter()
                .map(|(id, document)| (*id, Arc::new(document.clone())))
                .collect(),
            &prior.ordinary,
            &prior.reach.machine,
            &self.functions,
            &self.helpers,
            &restore_selection,
        )
        .map_err(|error| refused(error.to_string()))?;
        let pinned_closure = PageId(old.closure.reference().0);
        let mut predecessor_code = match &restore_selection {
            lash_kernel_doc::Selection::Restored(selection) => selection.live.clone(),
            lash_kernel_doc::Selection::Fresh => {
                let request = RootLeaseRequest {
                    owner: RootOwner::Exec(exec.clone()),
                    expected: row.root,
                    fence: LeaseFence::Reader,
                    ttl_millis: 600_000,
                };
                let source = leased_pages(Arc::clone(&pages), request.clone()).await?;
                let live = tokio::task::spawn_blocking(move || {
                    let mut source = source;
                    let answer = source
                        .read(&[PageRef {
                            id: PageId(parked.page.root.0),
                            grant: PageGrant::NONE,
                        }])?
                        .remove(0)?;
                    let checkpoint = Checkpoint::of_answer(&answer)?;
                    lash_kernel_state::paged::checkpoint_code(&checkpoint, Box::new(source))
                })
                .await
                .map_err(|error| HeapFault::Transport(error.to_string()))??;
                let mut predecessor_code = live.callables;
                for (link, unit) in live.frames {
                    if link == start.link {
                        continue;
                    }
                    let lash_vocabulary::sites::Unit::Library(function) = unit else {
                        return Err(refused("foreign frame has no defining callable"));
                    };
                    predecessor_code.insert(lash_vocabulary::CallableId { link, function });
                }
                predecessor_code
            }
        };
        let next = successor.unwrap_or(start);
        let mut next_content = carry_snapshot(&self.documents, next.link).await?;
        let selected = next_content
            .inspect(
                next.link,
                &lash_kernel_package::ReachSelection::Start(next.selector.clone()),
                self.helpers.as_ref(),
            )
            .map_err(|e| refused(e.to_string()))?;
        predecessor_code.retain(|callable| callable.link != next.link);
        let mut selected = selected;
        for callable in &predecessor_code {
            if !next_content.links.contains_key(&callable.link) {
                let held = carry_snapshot(&self.documents, callable.link).await?;
                next_content.documents.extend(held.documents);
                next_content.links.extend(held.links);
                next_content.packages.extend(held.packages);
            }
            let debt = carried_reach(&next_content, *callable, &restore_selection, &self.helpers)
                .map_err(|error| refused(error.to_string()))?;
            selected.machine.extend(&debt.machine);
            for (link, functions) in debt.functions {
                selected
                    .functions
                    .entry(link)
                    .or_default()
                    .extend(functions);
            }
        }
        let selected_ids: std::collections::BTreeSet<_> =
            selected.functions.values().flatten().copied().collect();
        let stored: BTreeMap<_, _> = next_content
            .documents
            .values()
            .flat_map(|d| &d.functions)
            .filter(|(id, _)| selected_ids.contains(id))
            .map(|(id, f)| (*id, Arc::new(f.clone())))
            .collect();
        // The successor's fresh reach and the carried exact live facts form
        // one executable selection. Retained predecessor manifests may still
        // mention removed helpers that are outside this continuation.
        let successor_selection = carried_selection(
            &restore_selection,
            &selected.functions,
            start.link,
            next.link,
            &self.helpers,
        );
        let library = lash_kernel_doc::LibraryDependencies(
            next_content
                .documents
                .values()
                .flat_map(|document| document.library.iter().copied())
                .chain(match &successor_selection {
                    lash_kernel_doc::Selection::Restored(restored) => {
                        restored.pins.iter().copied().collect::<Vec<_>>()
                    }
                    lash_kernel_doc::Selection::Fresh => Vec::new(),
                })
                .collect(),
        );
        let held = redeclared.unwrap_or(&self.functions);
        let declared = lash_vm_broker::kernel::loader::declare(
            selected_ids.iter().copied(),
            rewritten.document.kernel(),
            held,
            &stored,
            &self.helpers,
            &library,
            &successor_selection,
        )
        .map_err(|e| refused(e.to_string()))?;
        let closure = lash_vm_broker::kernel::closure::derive(
            selected_ids,
            held,
            &stored,
            &self.helpers,
            &library,
        )
        .map_err(|e| refused(e.to_string()))?;
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
            lash_kernel_vm::PreparedLibrary::with_helpers(
                Arc::new(declared),
                Arc::clone(&self.helpers),
            ),
            successor_selection,
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
            lash_kernel_migrate::CarryContext {
                kernel: pinned_document.kernel(),
                closure: pinned_closure,
                links: next_content.links.clone(),
                selection: restore_selection,
            },
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
            held.admission.0.identity = lash_kernel_migrate::carry_effect(
                &correspondence,
                &held.admission.0.identity,
                &self.helpers,
            )?;
        }
        for wait in state.waits.values_mut() {
            let super::state::Wait::Step { identity, .. } = wait;
            *identity =
                lash_kernel_migrate::carry_effect(&correspondence, identity, &self.helpers)?;
        }
        for delivery in &mut state.settled {
            delivery.identity = lash_kernel_migrate::carry_effect(
                &correspondence,
                &delivery.identity,
                &self.helpers,
            )?;
        }
        Ok((nodes, predecessor_code))
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
    context: lash_kernel_migrate::CarryContext,
) -> Result<(NodeId, Vec<EncodedNode>), CarryFault> {
    let RootOwner::Exec(exec) = &request.owner else {
        return Err(refused("a checkpoint carry requires an execution owner"));
    };
    let owner = lash_vm_broker::kernel::graph::owner_binding_node(exec)
        .map_err(|error| refused(error.to_string()))?;
    let pages = leased_pages(store, request).await?;
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
        let documents = program
            .instances
            .iter()
            .filter_map(|(link, instance)| {
                program
                    .documents
                    .get(&instance.document)
                    .map(|document| (*link, document.as_ref().clone()))
            })
            .collect();
        let imports = program
            .instances
            .iter()
            .flat_map(|(link, instance)| {
                instance
                    .slots
                    .iter()
                    .filter_map(move |(slot, fill)| match fill {
                        lash_kernel_vm::PreparedSlot::Function { callable, .. } => {
                            Some(((*link, *slot), *callable))
                        }
                        _ => None,
                    })
            })
            .collect();
        // A parent-produced mismatch proof is membership-only here. A direct
        // carry caller still performs compatibility before the first write.
        match &context.selection {
            lash_kernel_doc::Selection::Restored(selection) => {
                lash_kernel_state::paged::check_helper_compatibility(
                    checkpoint.header().helper_library,
                    || Ok(lash_kernel_state::paged::HelperPins(selection.pins.clone())),
                    program.helpers(),
                )
                .map_err(carry_helper_refusal)?;
            }
            lash_kernel_doc::Selection::Fresh => {
                checkpoint
                    .check_helper_compatibility(
                        Box::new(source.clone()),
                        &documents,
                        &imports,
                        program.helpers(),
                    )
                    .map_err(carry_helper_refusal)?;
            }
        }
        let (page, checkpoint) = lash_kernel_migrate::carry_checkpoint(
            &mut source,
            &checkpoint,
            &rewritten,
            program.closure,
            program.link,
            &context,
            program.helpers(),
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

pub(super) async fn leased_pages(
    store: Arc<dyn PageStore>,
    request: RootLeaseRequest,
) -> Result<Pages, CarryFault> {
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
    Ok(Pages {
        lease: Arc::new(Lease {
            store,
            lease,
            runtime,
            _release: release,
        }),
        nodes: Arc::default(),
    })
}

fn carry_helper_refusal(error: lash_kernel_state::paged::HelperCompatibilityError) -> CarryFault {
    match error {
        lash_kernel_state::paged::HelperCompatibilityError::Heap(fault) => CarryFault::Heap(fault),
        lash_kernel_state::paged::HelperCompatibilityError::MissingHelper { function } => {
            CarryFault::Refused(ParkedRefusal::MissingHelper { function })
        }
    }
}

fn carried_reach(
    content: &lash_kernel_package::LinkedContent,
    callable: lash_vocabulary::CallableId,
    selection: &lash_kernel_doc::Selection,
    helpers: &lash_kernel_doc::BuildHelpers,
) -> Result<lash_kernel_package::LinkedReach, lash_kernel_package::LinkRefusal> {
    match selection {
        lash_kernel_doc::Selection::Fresh => content.inspect(
            callable.link,
            &lash_kernel_package::ReachSelection::Callable(callable.function),
            helpers,
        ),
        lash_kernel_doc::Selection::Restored(restored) => {
            content.inspect_restored(callable.link, restored, helpers)
        }
    }
}

/// Project producer-owned selected facts without re-deriving predecessor
/// manifest ancestry. Saved callables keep their defining Links; active
/// helper frames additionally gain the successor's operation context.
fn carried_selection(
    prior: &lash_kernel_doc::Selection,
    selected: &BTreeMap<
        lash_vocabulary::LinkId,
        std::collections::BTreeSet<lash_vocabulary::FunctionId>,
    >,
    predecessor: lash_vocabulary::LinkId,
    successor: lash_vocabulary::LinkId,
    helpers: &lash_kernel_doc::BuildHelpers,
) -> lash_kernel_doc::Selection {
    let lash_kernel_doc::Selection::Restored(prior) = prior else {
        return lash_kernel_doc::Selection::Fresh;
    };
    let mut restored = prior.clone();
    for (link, functions) in selected {
        for function in functions {
            restored.live.insert(lash_vocabulary::CallableId {
                link: *link,
                function: *function,
            });
            if helpers.contains(function) {
                restored.pins.insert(*function);
            }
        }
    }
    restored.live.extend(
        prior
            .live
            .iter()
            .filter(|callable| {
                callable.link == predecessor && prior.pins.contains(&callable.function)
            })
            .map(|callable| lash_vocabulary::CallableId {
                link: successor,
                function: callable.function,
            }),
    );
    lash_kernel_doc::Selection::Restored(restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-7169: retained predecessor manifests describe durable origins,
    /// not fresh successor admission. Exact live helper facts survive the join.
    #[test]
    fn a_carried_successor_ignores_dead_predecessor_helper_manifests() {
        use lash_kernel_doc::{
            BuildHelpers, Document, FunctionRegistry, NumberPolicy, RestoreSelection, Selection,
        };
        use lash_vocabulary::{CallableId, FunctionId, Link};
        let mut registry = FunctionRegistry::new();
        let member = registry
            .register(
                lash_kernel_doc::parse_definition(
                    "function carried.child() -> Any\nkernel 1\ncharge 1\nbody { return 7 }",
                )
                .unwrap(),
                None,
            )
            .unwrap();
        let helpers = Arc::new(BuildHelpers::new(registry.clone(), [member]).unwrap());
        let mut before = Document::new(NumberPolicy::BySpelling, vec![]);
        before.library.insert(FunctionId::from_bytes([47; 32]));
        let after = Document::new(NumberPolicy::BySpelling, vec![]);
        let old = Link {
            document: before.identity(),
            fills: BTreeMap::new(),
        };
        let next = Link {
            document: after.identity(),
            fills: BTreeMap::new(),
        };
        let prior = Selection::Restored(RestoreSelection {
            pins: [member].into(),
            live: [CallableId {
                link: old.identity(),
                function: member,
            }]
            .into(),
        });
        let content = lash_kernel_package::LinkedContent {
            documents: [
                (before.identity(), before.clone()),
                (after.identity(), after.clone()),
            ]
            .into(),
            links: [
                (old.identity(), old.clone()),
                (next.identity(), next.clone()),
            ]
            .into(),
            packages: BTreeMap::new(),
        };
        let debt = carried_reach(
            &content,
            CallableId {
                link: old.identity(),
                function: member,
            },
            &prior,
            &helpers,
        );
        assert!(
            debt.is_ok(),
            "retained callable admission uses its live proof: {debt:?}"
        );
        let selected = carried_selection(
            &prior,
            &BTreeMap::new(),
            old.identity(),
            next.identity(),
            &helpers,
        );
        let program = lash_kernel_vm::Program::prepare(
            next.identity(),
            [
                (before.identity(), Arc::new(before)),
                (after.identity(), Arc::new(after)),
            ]
            .into(),
            &[(old.identity(), old), (next.identity(), next)].into(),
            PageId([8; 32]),
            lash_kernel_vm::PreparedLibrary::with_helpers(Arc::new(registry), helpers),
            selected,
        );
        assert!(
            program.is_ok(),
            "the dead predecessor manifest cannot veto a carried live helper: {program:?}"
        );
    }
}
