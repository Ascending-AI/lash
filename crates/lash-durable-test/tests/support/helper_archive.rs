//! FIG-7512: exact helper code survives deployment, before and after a park.
use lash_core_store::checkpoint_graph::{EncodedNode, NodeId};
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_doc::{BuildHelpers, Datum, Document, FunctionRegistry, Selection};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_kernel_value::{FunctionId, Link};
use lash_kernel_vm::{KernelMachine, Machine, PreparedLibrary, Program, Step};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Clone, Default)]
struct Pages(Rc<RefCell<BTreeMap<NodeId, EncodedNode>>>);
impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|page| {
                let held = self.0.borrow();
                let node = held
                    .get(&NodeId(page.id.0))
                    .ok_or(HeapFault::Missing { page: page.id })?;
                lash_vm_broker::kernel::pages::answer_page(*page, &node.bytes, |_| {
                    Ok(PageGrant::NONE)
                })
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let node = lash_vm_broker::kernel::pages::frame_page(&body)?
            .named()
            .into_node();
        let page = PageRef {
            id: PageId(node.id.0),
            grant: PageGrant::NONE,
        };
        self.0.borrow_mut().insert(node.id, node);
        Ok(page)
    }
}
struct Host;
impl lash_kernel_vm::Host for Host {
    fn print(&mut self, _: &Datum) {
        panic!("no prints")
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}
fn bounds() -> lash_kernel_vm::Bounds {
    lash_kernel_vm::Bounds {
        memory_meter: lash_kernel_vm::MemoryMeter::On,
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: 1_000_000,
        memory: 64 << 20,
        call_depth: 100,
        live_tasks: 100,
        requests_per_park: 100,
        join_members: 100,
    }
}
struct Fixture {
    document: Arc<Document>,
    link: Link,
    old: lash_kernel_releases::HelperRelease,
    current: lash_kernel_releases::HelperRelease,
    archive: lash_kernel_releases::archive::Archive,
    helper: FunctionId,
}
impl Fixture {
    fn new() -> Self {
        use lash_kernel_releases::archive::{Archive, Binding, HelperId, Library, Release};
        let old_text = "function law.twice(f: Fn(x: Any) -> Any, x: Any) -> Any\nkernel 1\ncharge 5\nbody { let a = apply f(x) let b = apply f(a) return b }";
        let old_definition = lash_kernel_doc::parse_definition(old_text).unwrap();
        let helper = old_definition.identity();
        let definitions = BTreeMap::from([(helper, old_definition.clone())]);
        let library = Library {
            identity: lash_kernel_value::HelperLibraryId::of_members([helper]),
            members: [helper].into(),
            roots: [helper].into(),
        };
        let contract =
            lash_kernel_releases::archive::contract(helper, &definitions, &BTreeMap::new())
                .unwrap();
        let archive = Archive::default()
            .append(Release {
                library: library.clone(),
                definitions,
                libraries: [("fixture".to_owned(), library)].into(),
                bindings: [(
                    HelperId("fixture:law.twice".to_owned()),
                    Binding {
                        revision: contract.revision(),
                        function: helper,
                    },
                )]
                .into(),
                ..Default::default()
            })
            .unwrap();
        let release = |definition: lash_kernel_doc::FunctionDefinition| {
            let mut functions = FunctionRegistry::declarations();
            let id = functions.register(definition, None).unwrap();
            lash_kernel_releases::HelperRelease {
                helpers: Arc::new(BuildHelpers::new(functions.clone(), [id]).unwrap()),
                functions,
                roots: [id].into(),
                labels: Default::default(),
            }
        };
        let old = release(old_definition);
        let current = release(lash_kernel_doc::parse_definition(
            "function law.twice(f: Fn(x: Any) -> Any, x: Any) -> Any\nkernel 1\ncharge 50\nbody { return 99 }"
        ).unwrap());
        let callback = "function law.callback(x: Any) -> Any\nkernel 1\nmetering statements\nimport 0 operation law.echo(x: Any) -> Any\nbody { let answer = perform 0(x) as Any return answer }";
        let mut document = lash_kernel_doc::parse_document(&format!(
            "kernel 1\nnumbers by_spelling\nimport 0 operation law.echo(x: Any) -> Any\ndefinition {}\ndefinition {}\nmain {{ let value = invoke law.twice(&law.callback, 7) return value }}",
            serde_json::to_string(old_text).unwrap(), serde_json::to_string(callback).unwrap()
        )).unwrap();
        document.functions.remove(&helper);
        document.library = lash_kernel_doc::LibraryDependencies([helper].into());
        let fills = document
            .imports
            .iter()
            .map(|(slot, import)| {
                let lash_kernel_value::ImportKind::Operation { signature } = &import.kind else {
                    panic!("operation")
                };
                (
                    *slot,
                    lash_kernel_value::Fill::Operation(
                        lash_kernel_value::OperationRequirement::new(
                            signature.clone(),
                            lash_kernel_value::ContractId::new("1"),
                        ),
                    ),
                )
            })
            .collect();
        let link = Link {
            helper_fills: Default::default(),
            document: document.identity(),
            fills,
        };
        Self {
            document: Arc::new(document),
            link,
            old,
            current,
            archive,
            helper,
        }
    }
    fn upgraded(&self) -> lash_kernel_releases::HelperRelease {
        lash_kernel_releases::with_archive(
            self.current.clone(),
            &self.archive,
            &["fixture".to_owned()].into(),
        )
        .unwrap()
    }
    fn program(
        &self,
        release: &lash_kernel_releases::HelperRelease,
        selection: Selection,
    ) -> Program {
        let registry = Arc::new(release.functions.clone());
        let stored = self
            .document
            .functions
            .iter()
            .map(|(id, definition)| (*id, Arc::new(definition.clone())))
            .collect();
        let closure = lash_vm_broker::kernel::closure::derive(
            self.document.functions.keys().copied(),
            &registry,
            &stored,
            &release.helpers,
            &self.document.library,
        )
        .unwrap();
        Program::prepare(
            self.link.identity(),
            [(self.document.identity(), Arc::clone(&self.document))].into(),
            &[(self.link.identity(), self.link.clone())].into(),
            PageId(closure.reference().0),
            PreparedLibrary::with_helpers(registry, Arc::clone(&release.helpers)),
            selection,
        )
        .unwrap()
    }
    fn start(
        &self,
        release: &lash_kernel_releases::HelperRelease,
        pages: Pages,
        owner: PageId,
    ) -> KernelMachine {
        KernelMachine::start(
            self.program(release, Selection::Fresh),
            bounds(),
            lash_kernel_vm::CellStart {
                owner,
                target: lash_kernel_vm::Target::Main,
                args: vec![],
                session: None,
                modes: Default::default(),
                host: vec![],
            },
            Box::new(pages),
        )
        .unwrap()
    }
}
fn finish(mut machine: KernelMachine) -> (Datum, u64) {
    loop {
        match machine.run(&mut Host, u64::MAX).unwrap() {
            Step::Slice => {}
            Step::Parked(park) => {
                for request in park.requests {
                    let lash_kernel_vm::Request::Effect(effect) = request;
                    machine
                        .deliver(
                            effect.wait,
                            lash_kernel_vm::Outcome::Completed(Datum::Int(7.into())),
                        )
                        .unwrap();
                }
            }
            Step::Ended(lash_kernel_vm::End::Finished(done)) => {
                return (done.result, machine.meters().charged.unwrap());
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

pub(super) async fn crash_before_first_park(tier: super::served::Tier) {
    let mut fixture = Fixture::new();
    let accepted = fixture.document.identity();
    let original = finish(fixture.start(&fixture.old, Pages::default(), PageId([9; 32])));
    let (_, bytes, _) = persisted(tier, &fixture, &Pages::default(), None).await;
    let mut before_crash = fixture.start(&fixture.old, Pages::default(), PageId([9; 32]));
    assert!(matches!(
        before_crash.run(&mut Host, 1).unwrap(),
        Step::Slice
    ));
    drop(before_crash);
    let release = fixture.upgraded();
    let document: Document = lash_kernel_value::tokens::decode_body(&bytes).unwrap();
    assert_eq!(document.identity(), accepted);
    fixture.document = Arc::new(document);
    let retried = finish(fixture.start(&release, Pages::default(), PageId([9; 32])));
    assert_eq!(retried, original);
    assert_eq!(retried.0, Datum::Int(7.into()));
}

/// Persist the accepted document and optional checkpoint in the real tier's
/// fenced execution graph, then hydrate into a new page source after the writer dies.
async fn persisted(
    tier: super::served::Tier,
    fixture: &Fixture,
    pages: &Pages,
    checkpoint: Option<NodeId>,
) -> (Pages, Vec<u8>, Option<NodeId>) {
    use lash_core_execution::runtime::actor::process::state_graph::{
        read_engine_state, stage_engine_state,
    };
    use lash_core_store::checkpoint_graph::{
        CommitId, DecodeLimits, EncodeBudget, ExplicitLimit, GraphStaging,
    };
    use lash_durable::{ActorKey, CommitLabel, FormatSet, MailTx, NodeSpec};
    let (stores, _keep) = super::served::stores(tier).await.unwrap();
    let store = stores.durable_store();
    let process = lash_core::ProcessIdMint::sequential_id_for_testing(7512);
    let actor = ActorKey::process(process.as_str()).unwrap();
    let format = FormatSet::new("archived-helper-law");
    let mut mail = MailTx::new();
    mail.create_actor(actor.clone(), format.clone());
    store
        .commit_mail(mail, CommitLabel::new("law.create"))
        .await
        .unwrap();
    let node = store
        .register_node(&NodeSpec {
            node: lash_durable::NodeId::new("writer"),
            decodes: vec![format],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 600_000,
        })
        .await
        .unwrap();
    let claim = store.claim(&node, 1).await.unwrap().remove(0);
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    for page in pages.0.borrow().values() {
        staging.adopt(page.clone()).unwrap();
    }
    let registry = Arc::new(fixture.old.functions.clone());
    let definitions = fixture
        .document
        .functions
        .iter()
        .map(|(id, definition)| (*id, Arc::new(definition.clone())))
        .collect();
    let closure = lash_vm_broker::kernel::closure::derive(
        fixture.document.functions.keys().copied(),
        &registry,
        &definitions,
        &fixture.old.helpers,
        &fixture.document.library,
    )
    .unwrap();
    if checkpoint.is_some() {
        for node in closure.nodes().values() {
            staging.adopt(node.clone()).unwrap();
        }
    }
    let state = lash_execution_contracts::process::EngineState {
        staged: vec![],
        format: lash_execution_contracts::process::EngineStateFormat {
            kind: "helper-law".into(),
            version: 1,
        },
        bytes: fixture.document.canonical_bytes(),
        pages: checkpoint,
        requirements: Default::default(),
    };
    let (_, write) = stage_engine_state(
        &mut staging,
        &process,
        "helper-law",
        &state,
        (None, None),
        CommitId::mint(),
    )
    .unwrap();
    let mut tx = store.begin(&actor, claim.epoch).await.unwrap();
    tx.stage_all(staging).write(write);
    store
        .commit(tx, CommitLabel::new("law.accept"))
        .await
        .unwrap();
    let reads = store;
    let (row, _, restored) = read_engine_state(
        reads.as_ref(),
        &process,
        DecodeLimits::within_total(u64::MAX),
    )
    .await
    .unwrap()
    .unwrap();
    // Host graph hydration deliberately stops at a semantic heap page. Fetch
    // the heap under its root lease, as the existing cold carry oracle does.
    let loaded = Pages::default();
    if let Some(root) = restored.pages {
        use lash_durable::domain::pages::{LeaseFence, RootLeaseRequest};
        let page_store = reads.pages().unwrap();
        let lease = page_store
            .acquire_root_lease(RootLeaseRequest {
                owner: lash_durable::domain::RootOwner::Exec(row.exec),
                expected: row.root,
                fence: LeaseFence::Reader,
                ttl_millis: 600_000,
            })
            .await
            .unwrap();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            if loaded.0.borrow().contains_key(&id) {
                continue;
            }
            let stored = page_store
                .read_pages(&lease, &[id])
                .await
                .unwrap()
                .remove(0);
            let node = EncodedNode {
                id,
                bytes: stored.bytes,
            };
            pending.extend(node.envelope().unwrap().children.iter());
            loaded.0.borrow_mut().insert(id, node);
        }
        page_store.release_root_lease(lease.id).await.unwrap();
    }
    (loaded, restored.bytes, restored.pages)
}

pub(super) async fn run_across_deploy(tier: super::served::Tier) {
    let fixture = Fixture::new();
    let process = lash_core::ProcessIdMint::sequential_id_for_testing(7512);
    let owner = PageId(
        lash_vm_broker::kernel::graph::owner_binding_node(&lash_durable::domain::ExecKey::Process(
            process,
        ))
        .unwrap()
        .0,
    );
    let expected = finish(fixture.start(&fixture.old, Pages::default(), owner));
    let pages = Pages::default();
    let mut original = fixture.start(&fixture.old, pages.clone(), owner);
    let Step::Parked(park) = original.run(&mut Host, u64::MAX).unwrap() else {
        panic!("inside callback")
    };
    let lash_kernel_vm::Request::Effect(effect) = &park.requests[0];
    let wait = effect.wait;
    let sealed = original.checkpoint().unwrap();
    let (mut loaded, bytes, root) =
        persisted(tier, &fixture, &pages, Some(NodeId(sealed.page.id.0))).await;
    drop(original);
    let root = root.unwrap();
    let answer = loaded
        .read(&[PageRef {
            id: PageId(root.0),
            grant: PageGrant::NONE,
        }])
        .unwrap()
        .remove(0)
        .unwrap();
    let checkpoint: lash_kernel_state::paged::Checkpoint =
        lash_kernel_value::tokens::decode_body(&answer.tokens).unwrap();
    let document: Document = lash_kernel_value::tokens::decode_body(&bytes).unwrap();
    assert_eq!(document.identity(), fixture.document.identity());
    let release = fixture.upgraded();
    let (source, selection) = checkpoint
        .check_helper_compatibility(
            Box::new(loaded),
            &[(fixture.link.identity(), document)].into(),
            &Default::default(),
            &release.helpers,
        )
        .unwrap();
    let mut restored = KernelMachine::open(
        fixture.program(&release, selection),
        bounds(),
        &checkpoint,
        source,
    )
    .unwrap();
    restored
        .deliver(
            wait,
            lash_kernel_vm::Outcome::Completed(Datum::Int(7.into())),
        )
        .unwrap();
    assert_eq!(finish(restored), expected);
}

pub(super) async fn rolling_nodes(tier: super::served::Tier) {
    use lash_durable::{ActorKey, CommitLabel, FormatSet, MailTx, NodeSpec, Release};
    use lash_vocabulary::claims::{ClaimRequirements, NodeCapabilities};
    let fixture = Fixture::new();
    let content = lash_kernel_package::LinkedContent {
        documents: [(fixture.document.identity(), Arc::clone(&fixture.document))].into(),
        links: [(fixture.link.identity(), fixture.link.clone())].into(),
        ..Default::default()
    };
    let reach = content
        .inspect(
            fixture.link.identity(),
            &lash_kernel_package::ReachSelection::Start(lash_kernel_value::StartSelector::Main),
            fixture.old.helpers.as_ref(),
        )
        .unwrap();
    let mut requirements = ClaimRequirements {
        machine: reach.machine,
        ..Default::default()
    };
    assert_eq!(
        requirements.machine.helpers.outside,
        [(
            fixture.old.helpers.current_identity(),
            [fixture.helper].into()
        )]
        .into()
    );
    // This token is already persisted by binaries predating per-helper claim debt.
    requirements
        .machine
        .libraries
        .insert(fixture.old.helpers.identity());
    let (stores, _keep) = super::served::stores(tier).await.unwrap();
    let store = stores.durable_store();
    let format = FormatSet::new("helper-claim-law");
    let capabilities = |release: &lash_kernel_releases::HelperRelease| NodeCapabilities {
        machine: lash_kernel_value::claims::NodeRequirements {
            helpers: release.helpers.claim_capabilities(),
            libraries: release.helpers.library_identities().clone(),
            kernels: [1].into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let old_capabilities = capabilities(&fixture.current);
    let node = |name: &str, capabilities| NodeSpec {
        node: lash_durable::NodeId::new(name),
        decodes: vec![format.clone()],
        serves: vec![],
        capabilities,
        ttl_millis: 600_000,
    };
    let older = store
        .register_node(&node("without-archive", old_capabilities.clone()))
        .await
        .unwrap();
    let actor = ActorKey::process("archive-claim").unwrap();
    let mut mail = MailTx::new();
    mail.create_actor(actor.clone(), format.clone());
    store
        .commit_mail(mail, CommitLabel::new("law.create"))
        .await
        .unwrap();
    let claim = store.claim(&older, 1).await.unwrap().remove(0);
    let mut tx = store.begin(&actor, claim.epoch).await.unwrap();
    tx.set_requires(requirements.tokens().collect())
        .give_up(Release::Ready);
    store
        .commit(tx, CommitLabel::new("law.record"))
        .await
        .unwrap();
    assert!(store.claim(&older, 1).await.unwrap().is_empty());
    let missing = requirements.unmet_by(&old_capabilities);
    assert_eq!(missing.machine.helpers, requirements.machine.helpers);
    assert!(missing.machine.natives.is_empty());
    let capable = store
        .register_node(&node("with-archive", capabilities(&fixture.upgraded())))
        .await
        .unwrap();
    let claimed = store.claim(&capable, 1).await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].actor, actor);
    let mut tx = store.begin(&actor, claimed[0].epoch).await.unwrap();
    let archive = lash_kernel_value::HelperLibraryId::of_members([fixture.helper]);
    requirements.machine.helpers = lash_kernel_value::claims::HelperRequirements {
        archives: [(archive, 2)].into(),
        outside: Default::default(),
    };
    tx.set_requires(requirements.tokens().collect())
        .give_up(Release::Ready);
    store
        .commit(tx, CommitLabel::new("law.generation"))
        .await
        .unwrap();
    let mut previous = capabilities(&fixture.upgraded());
    previous.machine.helpers.archives = [(archive, 1)].into();
    let previous = store
        .register_node(&node("generation-one", previous))
        .await
        .unwrap();
    assert!(store.claim(&previous, 1).await.unwrap().is_empty());
    assert_eq!(
        requirements
            .unmet_by(&previous.capabilities)
            .machine
            .helpers
            .archives,
        [(archive, 2)].into()
    );
    let mut next = capabilities(&fixture.upgraded());
    next.machine.helpers.archives = [(archive, 3)].into();
    let next = store
        .register_node(&node("generation-three", next))
        .await
        .unwrap();
    let claimed = store.claim(&next, 1).await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].actor, actor);
}
