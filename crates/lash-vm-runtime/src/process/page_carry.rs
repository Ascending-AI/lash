//! Parked pages prepared privately, then handed to the same owner commit
//! as their new engine state. A reader lease owns every await and blocking
//! traversal; dropping a preparation releases its lease and private pages.
use super::{KernelProcessEngine, state::KernelEngineState};
use lash_core_store::checkpoint_graph::{EncodedNode, NodeId};
use lash_durable::domain::pages::{LeaseFence, PageStore, RootLease, RootLeaseRequest};
use lash_durable::domain::{ExecKey, RootOwner};
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_migrate::{CarryFault, ParkedRefusal, Rewritten};
use lash_kernel_state::paged::run::rewrite::{LiveCode, RunCorrespondence};
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
    link: lash_kernel_value::LinkId,
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
        successor: &super::InlineRequest,
        retained: &std::collections::BTreeSet<lash_kernel_value::CallableId>,
        uses: &lash_execution_contracts::process::UseCorrespondence,
    ) -> Result<
        (
            Vec<EncodedNode>,
            std::collections::BTreeSet<lash_kernel_value::CallableId>,
        ),
        CarryFault,
    > {
        let Some(park) = state.progress.park_mut() else {
            return Ok(Default::default());
        };
        let input = super::state::KernelProcessInput::from_payload(&state.payload)
            .map_err(|error| refused(error.to_string()))?;
        let stores = self
            .carry_stores
            .as_ref()
            .ok_or_else(|| refused("this engine has no carry stores"))?;
        let prepared = self
            .prepare_checkpoint_carry(
                stores.as_ref(),
                &ExecKey::Process(process.clone()),
                park.run.page.root,
                &input.code,
                successor,
                rewritten,
                retained,
                &Default::default(),
                uses,
            )
            .await?;
        park.run.page = super::state::ParkedPage {
            root: prepared.root,
            kernel: rewritten.document.kernel(),
        };
        // Only code requirements change here. The owner derives link
        // requirements from the successor env at the same commit that
        // repoints that env.
        park.requirements.machine = prepared.machine;
        let (map, live) = (&prepared.map, &prepared.live);
        for held in state.admissions.values_mut() {
            held.admission.0.identity =
                lash_kernel_migrate::carry_effect(map, live, &held.admission.0.identity)?;
        }
        for wait in state.waits.values_mut() {
            let super::state::Wait::Step { identity, .. } = wait;
            *identity = lash_kernel_migrate::carry_effect(map, live, identity)?;
        }
        for delivery in &mut state.settled {
            delivery.identity = lash_kernel_migrate::carry_effect(map, live, &delivery.identity)?;
        }
        Ok((prepared.nodes, prepared.retained))
    }
    /// The single linked checkpoint preparation used by process and session
    /// succession. All pages remain private until the embedding owner's commit.
    /// Authored source roots remain closure inputs independently of executable
    /// reach, as they do when a session cold-loads its retained definitions.
    #[expect(
        clippy::too_many_arguments,
        reason = "one authenticated checkpoint succession"
    )]
    pub async fn prepare_checkpoint_carry(
        &self,
        stores: &dyn lash_execution_contracts::backend::StoreSet,
        exec: &ExecKey,
        root: NodeId,
        predecessor_code: &super::InlineRequest,
        successor: &super::InlineRequest,
        rewritten: &Rewritten,
        retained: &std::collections::BTreeSet<lash_kernel_value::CallableId>,
        authored: &std::collections::BTreeSet<lash_kernel_value::LinkId>,
        uses: &lash_execution_contracts::process::UseCorrespondence,
    ) -> Result<PreparedCheckpointCarry, CarryFault> {
        let (predecessor, _) = predecessor_code
            .linked()
            .ok_or_else(|| refused("unrealized predecessor code"))?;
        let (successor_link, successor_selector) = successor
            .linked()
            .ok_or_else(|| refused("unrealized successor code"))?;
        let prior_content = self
            .documents
            .load_execution_content(predecessor_code, retained)
            .await
            .map_err(|error| refused(error.to_string()))?;
        let pages = stores
            .durable_store()
            .pages()
            .ok_or_else(|| refused("this engine has no page store"))?;
        let row = pages
            .exec_root(exec)
            .await
            .map_err(|error| HeapFault::Transport(error.to_string()))?
            .ok_or_else(|| refused("the process has no execution root"))?;
        let selection = lash_kernel_doc::Selection::Fresh;
        let prior = self
            .documents
            .admit_execution(prior_content, &selection)
            .map_err(|error| refused(error.to_string()))?;
        let pinned_document = prior
            .content
            .document(
                prior
                    .content
                    .link(predecessor)
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
                .map(|(id, document)| (*id, Arc::clone(document)))
                .collect(),
            &prior.ordinary,
            &prior.reach.machine,
            &self.functions,
            &self.helpers,
            &selection,
        )
        .map_err(|error| refused(error.to_string()))?;
        let pinned_closure = PageId(old.closure.reference().0);
        let mut debt = {
            let request = RootLeaseRequest {
                owner: RootOwner::Exec(exec.clone()),
                expected: row.root,
                fence: LeaseFence::Reader,
                ttl_millis: 600_000,
            };
            let source = leased_pages(Arc::clone(&pages), request).await?;
            let live = tokio::task::spawn_blocking(move || {
                let mut source = source;
                let answer = source
                    .read(&[PageRef {
                        id: PageId(root.0),
                        grant: PageGrant::NONE,
                    }])?
                    .remove(0)?;
                let checkpoint = Checkpoint::of_answer(&answer)?;
                lash_kernel_migrate::run_code(&mut source, &checkpoint)
            })
            .await
            .map_err(|error| HeapFault::Transport(error.to_string()))??;
            let mut predecessor_code = live.callables;
            for (link, unit) in live.frames {
                if link == predecessor {
                    continue;
                }
                let lash_kernel_value::sites::Unit::Library(function) = unit else {
                    return Err(refused("foreign frame has no defining callable"));
                };
                predecessor_code.insert(lash_kernel_value::CallableId { link, function });
            }
            predecessor_code
        };
        let mut next_content = self
            .documents
            .load_execution_content(successor, &Default::default())
            .await
            .map_err(|error| refused(error.to_string()))?
            .content;
        // Authentication needs the predecessor's complete Link DAG, even
        // when an import is outside the successor's live executable reach.
        next_content.links.extend(prior.content.links.clone());
        next_content
            .documents
            .extend(prior.content.documents.clone());
        for link in authored {
            if !next_content.links.contains_key(link) {
                let held = carry_snapshot(&self.documents, *link).await?;
                next_content.documents.extend(held.documents);
                next_content.links.extend(held.links);
                next_content.packages.extend(held.packages);
            }
        }
        let selected = self
            .documents
            .inspect(
                &next_content,
                successor_link,
                &super::documents::selection(&successor_selector),
            )
            .map_err(|e| refused(e.to_string()))?;
        debt.retain(|callable| callable.link != successor_link);
        let mut selected = selected;
        for callable in &debt {
            if !next_content.links.contains_key(&callable.link) {
                let held = carry_snapshot(&self.documents, callable.link).await?;
                next_content.documents.extend(held.documents);
                next_content.links.extend(held.links);
                next_content.packages.extend(held.packages);
            }
            let debt = next_content
                .inspect(
                    callable.link,
                    &lash_kernel_package::ReachSelection::Callable(callable.function),
                    self.helpers.as_ref(),
                )
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
        let mut closure_functions = selected.functions.clone();
        for link in authored {
            let ordinary = next_content
                .inspect_ordinary(*link, &lash_kernel_package::ReachSelection::AllExports)
                .map_err(|error| refused(error.to_string()))?;
            for (link, functions) in ordinary.functions {
                closure_functions.entry(link).or_default().extend(functions);
            }
        }
        let assembled = lash_vm_broker::kernel::loader::declare_reach(
            &rewritten.document,
            &next_content
                .documents
                .iter()
                .map(|(id, doc)| (*id, Arc::clone(doc)))
                .collect(),
            &closure_functions,
            &selected.machine,
            &self.functions,
            &self.helpers,
            &selection,
        )
        .map_err(|error| refused(error.to_string()))?;
        let closure = assembled.closure;
        let mut program = lash_kernel_vm::Program::prepare(
            successor_link,
            next_content
                .documents
                .iter()
                .map(|(id, d)| (*id, Arc::clone(d)))
                .collect(),
            &next_content.links,
            PageId(closure.reference().0),
            lash_kernel_vm::PreparedLibrary::with_helpers(
                assembled.registry,
                Arc::clone(&self.helpers),
            ),
            lash_kernel_doc::Selection::Fresh,
        )
        .map_err(|e| refused(e.to_string()))?;
        program.functions = closure.body().identities().collect();
        let (page, mut nodes, map, live) = carry_run_pages(
            pages,
            RootLeaseRequest {
                owner: RootOwner::Exec(exec.clone()),
                expected: row.root,
                fence: LeaseFence::Reader,
                ttl_millis: 600_000,
            },
            root,
            rewritten.clone(),
            program,
            self.bounds,
            lash_kernel_migrate::CarryContext {
                kernel: pinned_document.kernel(),
                closure: pinned_closure,
                code: prior.ordinary.clone(),
                links: next_content.links.clone(),
                selection,
                retained: Default::default(),
                uses: uses.uses.clone(),
            },
        )
        .await?;
        nodes.extend(closure.nodes().values().cloned());
        Ok(PreparedCheckpointCarry {
            root: page,
            nodes,
            map,
            live,
            retained: debt,
            machine: selected.machine,
        })
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
    carry_run_pages(store, request, root, rewritten, program, bounds, context)
        .await
        .map(|(page, nodes, ..)| (page, nodes))
}

/// As [`carry_parked_checkpoint`], with the run's code and the
/// correspondence it was rewritten by, which the embedding rekeys its own
/// wait records with.
async fn carry_run_pages(
    store: Arc<dyn PageStore>,
    request: RootLeaseRequest,
    root: NodeId,
    rewritten: Rewritten,
    program: lash_kernel_vm::Program,
    bounds: lash_kernel_vm::Bounds,
    context: lash_kernel_migrate::CarryContext,
) -> Result<(NodeId, Vec<EncodedNode>, RunCorrespondence, LiveCode), CarryFault> {
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
        let parked = Checkpoint::of_answer(&answer)?;
        let header = parked.header();
        if header.owner != PageId(owner.0) {
            return Err(refused("checkpoint belongs to another execution owner"));
        }
        if header.kernel != context.kernel {
            return Err(ParkedRefusal::Version {
                found: header.kernel,
                from: context.kernel,
            }
            .into());
        }
        if header.closure != context.closure {
            return Err(refused("checkpoint closure differs from its pinned code"));
        }
        if header.document != rewritten.correspondence.base {
            return Err(ParkedRefusal::Document {
                parked: header.document,
                rewritten: rewritten.correspondence.base,
            }
            .into());
        }
        let live = lash_kernel_migrate::run_code(&mut source, &parked)?;
        for (link, unit) in &live.frames {
            match unit {
                lash_kernel_value::sites::Unit::Main
                | lash_kernel_value::sites::Unit::Function(_)
                    if *link == header.link => {}
                lash_kernel_value::sites::Unit::Library(function) => {
                    check_origin(*link, *function, &context, &program)?;
                }
                lash_kernel_value::sites::Unit::Main
                | lash_kernel_value::sites::Unit::Function(_) => {
                    return Err(refused("foreign frame has no authenticated defining code"));
                }
            }
        }
        for callable in &live.callables {
            check_origin(callable.link, callable.function, &context, &program)?;
        }
        for (before, after) in &context.uses {
            let old = context
                .links
                .get(&before.link)
                .and_then(|link| link.operation_fill(before.slot));
            let new = program.instances.get(&after.link).and_then(|instance| {
                context
                    .links
                    .get(&instance.link)
                    .and_then(|link| link.operation_fill(after.slot))
            });
            if old.is_none()
                || old != new
                || !matches!(
                    old,
                    Some(
                        lash_kernel_value::package::Fill::Operation(_)
                            | lash_kernel_value::package::Fill::Deferred { .. }
                    )
                )
            {
                return Err(refused(
                    "operation use correspondence changed an immutable fill",
                ));
            }
        }
        // The core's own open refuses a helper the live resume closure pins
        // and this image lacks (contract §7); the carry checks no helper.
        let map = run_correspondence(header.link, &live, &rewritten, &program)?;
        let page = lash_kernel_migrate::carry_run(
            &mut source,
            &parked,
            &map,
            lash_kernel_migrate::Successor {
                document: rewritten.identity(),
                kernel: program.document.kernel(),
                closure: program.closure,
                helper_library: program.helpers().identity(),
            },
        )?;
        let sealed = source.read(&[page])?.remove(0)?;
        let checkpoint = Checkpoint::of_answer(&sealed)?;
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
        Ok((NodeId(page.id.0), nodes, map, live))
    })
    .await
    .map_err(|error| HeapFault::Transport(error.to_string()))?
}

/// The run's active code ids onto the successor (`K-VER-005`): the
/// predecessor's Link onto the successor's, a library function onto its
/// redeclaration, a site of the predecessor's own code onto its survivor;
/// code of any other Link keeps its identity and must be in the successor.
/// Every callable the run holds keeps its exact defining Link and code.
fn run_correspondence(
    predecessor: lash_kernel_value::LinkId,
    live: &lash_kernel_state::paged::run::rewrite::LiveCode,
    rewritten: &Rewritten,
    program: &lash_kernel_vm::Program,
) -> Result<lash_kernel_state::paged::run::rewrite::RunCorrespondence, CarryFault> {
    use lash_kernel_state::paged::run::rewrite::SiteMap;
    let successor = program.link;
    let function =
        |id: lash_kernel_value::FunctionId| rewritten.functions.get(&id).copied().unwrap_or(id);
    let mut map = RunCorrespondence {
        sites: Some(SiteMap::new(rewritten.correspondence.clone())),
        ..RunCorrespondence::default()
    };
    map.links.insert(predecessor, successor);
    for link in live.links.iter().filter(|link| **link != predecessor) {
        if !program.instances.contains_key(link) {
            return Err(refused(
                "live code's defining Link is absent from the successor",
            ));
        }
        map.links.insert(*link, *link);
    }
    for id in &live.functions {
        map.functions.insert(*id, function(*id));
    }
    map.retained = live
        .callables
        .iter()
        .map(|callable| callable.link)
        .collect();
    Ok(map)
}

pub(super) async fn leased_pages(
    store: Arc<dyn PageStore>,
    request: RootLeaseRequest,
) -> Result<Pages, CarryFault> {
    let lease = lash_durable::fenced_read::acquire_lease(store.as_ref(), request)
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

fn check_origin(
    link: lash_kernel_value::LinkId,
    function: lash_kernel_value::FunctionId,
    context: &lash_kernel_migrate::CarryContext,
    program: &lash_kernel_vm::Program,
) -> Result<(), CarryFault> {
    let document = context
        .links
        .get(&link)
        .filter(|instance| instance.identity() == link)
        .and_then(|instance| program.documents.get(&instance.document))
        .ok_or_else(|| refused("live code has no pinned defining Link"))?;
    if context
        .code
        .get(&link)
        .is_some_and(|functions| functions.contains(&function))
        && document.functions.contains_key(&function)
    {
        return Ok(());
    }
    let callable = lash_kernel_value::CallableId { link, function };
    if context
        .selection
        .admits_helper(&callable, &document.library, program.helpers())
    {
        return Ok(());
    }
    Err(lash_kernel_migrate::ParkedRefusal::FunctionNotCarried { function }.into())
}

/// Private preparation, transferred intact to the fenced execution owner.
pub struct PreparedCheckpointCarry {
    pub root: NodeId,
    pub nodes: Vec<EncodedNode>,
    /// The correspondence the run was rewritten by.
    pub map: RunCorrespondence,
    /// The code the run named before it was rewritten.
    pub live: LiveCode,
    pub retained: std::collections::BTreeSet<lash_kernel_value::CallableId>,
    pub machine: lash_kernel_value::claims::NodeRequirements,
}

impl KernelProcessEngine {
    /// Whether a helper upgrade may proceed over this process's parked run.
    /// Code the run holds as a value is frozen with the helpers it records,
    /// so a changed one refuses; a changed helper only a resume point pins
    /// defers the upgrade until the run drains.
    pub(super) async fn check_helper_upgrade(
        &self,
        process: &lash_core::ProcessId,
        state: &KernelEngineState,
        upgrade: &lash_kernel_doc::HelperUpgrade,
    ) -> Result<
        lash_kernel_doc::HelperUpgradeSafety,
        lash_execution_contracts::process::SuccessionError,
    > {
        use lash_execution_contracts::process::SuccessionError;
        let unavailable = |error: String| SuccessionError::Unavailable(error);
        let Some(root) = state.progress.park().map(|park| park.run.page.root) else {
            return Ok(lash_kernel_doc::HelperUpgradeSafety::Safe);
        };
        let stores = self
            .carry_stores
            .as_ref()
            .ok_or_else(|| unavailable("helper upgrade requires carry stores".into()))?;
        let pages = stores
            .durable_store()
            .pages()
            .ok_or_else(|| unavailable("helper upgrade requires pages".into()))?;
        let exec = ExecKey::Process(process.clone());
        let row = pages
            .exec_root(&exec)
            .await
            .map_err(|e| unavailable(e.to_string()))?
            .ok_or_else(|| unavailable("helper upgrade execution root is missing".into()))?;
        let source = leased_pages(
            pages,
            RootLeaseRequest {
                owner: RootOwner::Exec(exec),
                expected: row.root,
                fence: LeaseFence::Reader,
                ttl_millis: 600_000,
            },
        )
        .await
        .map_err(|e| unavailable(e.to_string()))?;
        let live = tokio::task::spawn_blocking(move || {
            let mut source = source;
            let page = source
                .read(&[PageRef {
                    id: PageId(root.0),
                    grant: PageGrant::NONE,
                }])?
                .remove(0)?;
            let checkpoint = Checkpoint::of_answer(&page)?;
            lash_kernel_migrate::run_code(&mut source, &checkpoint)
        })
        .await
        .map_err(|e| unavailable(e.to_string()))?
        .map_err(|e| unavailable(e.to_string()))?;
        lash_kernel_migrate::check_run_upgrade(&live, upgrade)
            .map_err(super::succession::typed_refusal)
    }
}
