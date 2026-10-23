#![allow(clippy::expect_used, clippy::unwrap_used)]
//! K-VER-005, K-LIB-009 and ORIGIN against actual parked page records.
use lash::package::*;
use lash_core_store::checkpoint_graph::{EncodedNode, NodeId};
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_doc::{Datum, Document, FunctionDefinition, FunctionRegistry};
use lash_kernel_package::{LinkedContent, Namespace};
use lash_vocabulary::OperationRequirement;

use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_kernel_vm::{KernelMachine, Machine, Program, Step};
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
                let nodes = self.0.borrow();
                let bytes = &nodes
                    .get(&NodeId(page.id.0))
                    .ok_or(HeapFault::Missing { page: page.id })?
                    .bytes;
                lash_vm_broker::kernel::pages::answer_page(*page, bytes, |_| Ok(PageGrant::NONE))
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let node = lash_vm_broker::kernel::pages::frame_page(&body)?
            .named()
            .into_node();
        let reference = PageRef {
            id: PageId(node.id.0),
            grant: PageGrant::NONE,
        };
        self.0.borrow_mut().insert(node.id, node);
        Ok(reference)
    }
}
struct Host;
impl lash_kernel_vm::Host for Host {
    fn read(
        &mut self,
        _: &lash_kernel_doc::Handle,
        _: &Datum,
    ) -> Result<Datum, lash_kernel_doc::ErrorDatum> {
        panic!("no read")
    }
    fn print(&mut self, _: &Datum) {
        panic!("no print")
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
struct Extra;
impl lash_kernel_doc::NativeFunction for Extra {
    fn call(
        &self,
        _: lash_kernel_doc::NativeCall<'_, '_>,
    ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
        Ok(lash_kernel_doc::Value::Null)
    }
}
fn release(next: bool) -> Vec<FunctionDefinition> {
    let extra = lash_kernel_doc::parse_definition(
        "function pkg.extra() -> Any\nkernel 1\ncharge 1\nnative\n",
    )
    .unwrap();
    let called = if next {
        format!("use pkg.extra = @{}\n", extra.identity())
    } else {
        String::new()
    };
    let suffix = if next {
        "let unused = invoke pkg.extra()"
    } else {
        ""
    };
    let fetch = lash_kernel_doc::parse_definition(&format!(
        "function pkg.fetch(x: Any) -> Any\nkernel 1\ncharge 1\nimport 0 operation pkg.echo(x: Any) -> Any\n{called}body {{ let v = perform 0(x) as Any {suffix} return v }}"
    )).unwrap();
    let start = lash_kernel_doc::parse_definition(&format!(
        "function pkg.start(x: Any) -> Any\nkernel 1\ncharge 1\nuse pkg.fetch = @{}\nbody {{ let h = spawn invoke pkg.fetch(x) return h }}", fetch.identity()
    )).unwrap();
    let mut definitions = vec![fetch, start];
    if next {
        definitions.push(extra);
    }
    definitions
}
fn document(definitions: &[FunctionDefinition], next: bool) -> Document {
    let written = definitions
        .iter()
        .map(|definition| {
            format!(
                "definition {}\n",
                serde_json::to_string(&lash_kernel_doc::print_definition(definition)).unwrap()
            )
        })
        .collect::<String>();
    // The successor authors a different local closure. The already-created
    // value must still execute the predecessor's body and operation slot.
    let added = if next { "let newly_authored = 99" } else { "" };
    lash_kernel_doc::parse_document(&format!(
        "kernel 1\nnumbers float\nprivate captured\nimport 0 operation pkg.echo(x: Any) -> Any\n{written}main {{ let captured = [7] let f = fn() {{ {added} let echoed = perform 0(captured[0]) as Any return echoed }} let h = invoke pkg.start(7) let v = join h let saved = apply f() let tail = perform 0(7) as Any return [v, saved] }}"
    )).unwrap()
}
fn registry(
    definitions: impl IntoIterator<Item = FunctionDefinition>,
    declarations: bool,
) -> Arc<FunctionRegistry> {
    let mut functions = if declarations {
        FunctionRegistry::declarations()
    } else {
        FunctionRegistry::new()
    };
    let mut remaining: BTreeMap<_, _> =
        definitions.into_iter().map(|f| (f.identity(), f)).collect();
    while !remaining.is_empty() {
        let id = *remaining
            .iter()
            .find(|(_, f)| {
                f.body()
                    .is_none_or(|body| body.functions.keys().all(|id| functions.get(id).is_some()))
            })
            .expect("acyclic fixture definitions")
            .0;
        let definition = remaining.remove(&id).unwrap();
        let native = (definition.has_native() && !declarations)
            .then(|| Arc::new(Extra) as Arc<dyn lash_kernel_doc::NativeFunction>);
        functions.register(definition, native).unwrap();
    }
    Arc::new(functions)
}
fn program(
    content: &LinkedContent,
    root: LinkId,
    functions: Arc<FunctionRegistry>,
    closure: PageId,
) -> Program {
    Program::prepare(
        root,
        content
            .documents
            .iter()
            .map(|(id, d)| (*id, Arc::new(d.clone())))
            .collect(),
        &content.links,
        closure,
        lash_kernel_vm::PreparedLibrary::new(functions),
        lash_kernel_doc::Selection::Fresh,
    )
    .unwrap()
}
fn rewritten(
    before: &Document,
    after: &Document,
    a: &[FunctionDefinition],
    b: &[FunctionDefinition],
) -> lash_kernel_migrate::Rewritten {
    use lash_kernel_doc::{Correspondence, Node, Survivor};
    use lash_vocabulary::sites::{Site, Unit};
    let mut functions: BTreeMap<_, _> = a
        .iter()
        .zip(b)
        .map(|(a, b)| (a.identity(), b.identity()))
        .collect();
    let local = |document: &Document, definitions: &[FunctionDefinition]| {
        *document
            .functions
            .keys()
            .find(|id| !definitions.iter().any(|f| f.identity() == **id))
            .unwrap()
    };
    functions.insert(local(before, a), local(after, b));
    let mut entries = Vec::new();
    for (unit, block) in before.units() {
        let to = match &unit {
            Unit::Library(id) => Unit::Library(*functions.get(id).unwrap()),
            other => other.clone(),
        };
        let mut pending = vec![(vec![], Node::Block(block))];
        while let Some((path, node)) = pending.pop() {
            if let Node::Block(block) = node {
                let mut end = path.clone();
                end.push(block.len() as u32);
                entries.push(Survivor::new(
                    Site::new(unit.clone(), end.clone()),
                    Site::new(to.clone(), end),
                    false,
                ));
            }
            for (i, child) in (0u32..).zip(node.children()) {
                let mut path = path.clone();
                path.push(i);
                pending.push((path, child));
            }
            entries.push(Survivor::new(
                Site::new(unit.clone(), path.clone()),
                Site::new(to.clone(), path),
                true,
            ));
        }
    }
    lash_kernel_migrate::Rewritten {
        document: after.clone(),
        correspondence: Correspondence::of(before.identity(), after.identity(), entries).unwrap(),
        functions,
    }
}
async fn publish(
    stores: &Arc<dyn lash_execution_contracts::backend::StoreSet>,
    document: &Document,
    contract: &str,
) -> (LinkedContent, LinkId) {
    let ImportKind::Operation { signature } = &document.imports[&ImportSlot(0)].kind else {
        unreachable!()
    };
    let package = Package {
        path: PackagePath::parse("pkg").unwrap(),
        members: BTreeMap::from([(
            Segment::new("echo").unwrap(),
            Member {
                visibility: Visibility::Public,
                kind: MemberKind::Operation(OperationOffer {
                    requirement: OperationRequirement::new(
                        signature.clone(),
                        lash_vocabulary::ContractId::new(contract),
                    ),
                    intents: Default::default(),
                    controls: lash_vocabulary::TurnControls::none(),
                    mode: lash::package::OperationMode::Waited,
                }),
            },
        )]),
    };
    let context = PublicationContext {
        domain: PublicationDomain("paged-carry".into()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("owner".into()),
        root: Segment::new("pkg").unwrap(),
    };
    let advertisement = PackageAdvertisement::Enumerated(package.clone());
    let mut content = LinkedContent::default();
    let id = content
        .insert_document(document.clone(), &document.functions)
        .unwrap();
    let attachment = Attachment {
        packages: [package.path.clone()].into(),
        members: Default::default(),
        deferred: Default::default(),
    };
    let link = Namespace {
        advertisements: vec![(&context, &advertisement)],
    }
    .fulfil(&content, id, &context, None, Some(&attachment))
    .unwrap();
    let root = content.insert_link(link.clone()).unwrap();
    let claim = lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
        lash_core::HostArtifactPin::mint(),
    ))
    .unwrap();
    let documents = lash_vm_runtime::KernelDocuments::new(
        stores.module_artifacts(),
        stores.package_store(),
        std::sync::Arc::clone(
            lash_vm_client::packages::WorkerLibrary::built()
                .unwrap()
                .helpers(),
        ),
    );
    documents
        .admit_document(
            &claim,
            &lash_kernel_doc::DocumentBytes::from(document.clone()),
        )
        .await
        .unwrap();
    stores
        .package_store()
        .put_link(
            &claim,
            &lash_core_store::packages::LinkRecord { id: root, link },
            &[lash_core::ArtifactName {
                store: lash_core::ArtifactStoreId::KernelDocument,
                artifact_ref: id.to_string(),
            }],
        )
        .await
        .unwrap();
    stores
        .package_store()
        .put_package(
            &claim,
            &lash_core_store::packages::PackageRecord {
                id: package.identity(),
                package: package.clone(),
            },
        )
        .await
        .unwrap();
    stores
        .package_store()
        .publish(
            &claim,
            &PublicationRecord {
                context,
                path: package.path.clone(),
                content: PublishedContent::Package(package.identity()),
                code_links: Default::default(),
                imported_publications: vec![],
            },
        )
        .await
        .unwrap();
    (content, root)
}
fn park_with(
    program: Program,
    pages: &Pages,
    owner: PageId,
) -> (
    KernelMachine,
    lash_kernel_vm::SealedRoot,
    lash_kernel_vm::EffectRequest,
) {
    let mut machine = KernelMachine::start(
        program,
        bounds(),
        lash_kernel_vm::CellStart {
            owner,
            target: lash_kernel_vm::Target::Main,
            args: Vec::new(),
            session: None,
            modes: Default::default(),
            host: Vec::new(),
        },
        Box::new(pages.clone()),
    )
    .unwrap();
    let Step::Parked(park) = machine.run(&mut Host, u64::MAX).unwrap() else {
        panic!("park")
    };
    let lash_kernel_vm::Request::Effect(request) = park.requests[0].clone();
    let root = machine.checkpoint().unwrap();
    (machine, root, request)
}
fn finish(
    machine: &mut KernelMachine,
    wait: lash_vocabulary::sites::WaitId,
    retained: LinkId,
    root: LinkId,
) -> Datum {
    machine
        .deliver(
            wait,
            lash_kernel_vm::Outcome::Completed(Datum::Int(7.into())),
        )
        .unwrap();
    for link in [retained, root] {
        let Step::Parked(park) = machine.run(&mut Host, u64::MAX).unwrap() else {
            panic!("the next operation parks")
        };
        let [lash_kernel_vm::Request::Effect(request)] = park.requests.as_slice() else {
            panic!("one operation")
        };
        assert_eq!(
            request.use_,
            lash_vocabulary::OperationUse {
                link,
                slot: ImportSlot(0)
            },
            "preexisting closures keep their defining Link; resumed frames use the successor"
        );
        machine
            .deliver(
                request.wait,
                lash_kernel_vm::Outcome::Completed(Datum::Int(7.into())),
            )
            .unwrap();
    }
    match machine.run(&mut Host, u64::MAX).unwrap() {
        Step::Ended(lash_kernel_vm::End::Finished(result)) => result.result,
        other => panic!("{other:?}"),
    }
}

/// FIG-6233: the store admits the complete successor in one fenced
/// transaction. An uncommitted carry leaves the old pages and requirements;
/// a rejected commit leaves the same side. A committed carry roots B and
/// resumes with A retired. This law runs over both SQL backends.
async fn atomic_carry(tier: super::served::Tier) {
    use lash_core_execution::runtime::actor::process::state_graph::stage_engine_state;
    use lash_core_store::checkpoint_graph::{CommitId, EncodeBudget, ExplicitLimit, GraphStaging};
    use lash_durable::domain::pages::{LeaseFence, RootLeaseRequest};
    use lash_durable::domain::{ExecKey, RootOwner};
    use lash_durable::{ActorKey, CommitLabel, DomainWrite, FormatSet, MailTx, NodeSpec, Release};
    let (stores, _keep) = super::served::stores(tier)
        .await
        .expect("a required store tier");
    let store = stores.durable_store();
    let page_store = store.pages().unwrap();
    let format = FormatSet::new("paged-carry-law");
    let process = lash_core::ProcessIdMint::sequential_id_for_testing(6233);
    let actor = ActorKey::process(process.as_str()).unwrap();
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
    let exec = ExecKey::Process(process.clone());
    let owner = lash_vm_broker::kernel::graph::owner_binding_node(&exec).unwrap();
    let (a, b) = (release(false), release(true));
    let (document_a, document_b) = (document(&a, false), document(&b, true));
    let (mut content, link_a) = publish(&stores, &document_a, "A").await;
    let (next_content, link_b) = publish(&stores, &document_b, "B").await;
    let content_a = content.clone();
    content.documents.extend(next_content.documents);
    content.links.extend(next_content.links);
    let a_registry = registry(document_a.functions.values().cloned(), false);
    let b_registry = registry(
        content
            .documents
            .values()
            .flat_map(|d| d.functions.values().cloned()),
        false,
    );
    let closure_a = lash_vm_broker::kernel::closure::derive(
        document_a.functions.keys().copied(),
        &a_registry,
        &BTreeMap::new(),
        &lash_kernel_doc::BuildHelpers::empty(),
        &document_a.library,
    )
    .unwrap();
    let closure_b = lash_vm_broker::kernel::closure::derive(
        content
            .documents
            .values()
            .flat_map(|d| d.functions.keys().copied()),
        &b_registry,
        &BTreeMap::new(),
        &lash_kernel_doc::BuildHelpers::empty(),
        &document_b.library,
    )
    .unwrap();
    let pages = Pages::default();
    let (mut original, checkpoint, request) = park_with(
        program(
            &content_a,
            link_a,
            a_registry,
            PageId(closure_a.reference().0),
        ),
        &pages,
        PageId(owner.0),
    );
    let engine_state = |page, link, requirements| lash_execution_contracts::process::EngineState {
        staged: Vec::new(),
        format: lash_execution_contracts::process::EngineStateFormat {
            kind: lash_vm_runtime::LASH_VM_ENGINE_KIND.into(),
            version: 1,
        },
        bytes: serde_json::to_vec(&lash_vm_runtime::KernelProcessInput {
            code: lash_vm_runtime::process::InlineRequest::Linked {
                start: LinkedStart {
                    link,
                    selector: StartSelector::Main,
                },
            },
            args: Default::default(),
        })
        .unwrap(),
        pages: Some(page),
        requirements,
    };
    let requirements_a =
        lash_vm_broker::kernel::loader::requirements(&closure_a, 1, std::iter::empty());
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    for node in pages.0.borrow().values().chain(closure_a.nodes().values()) {
        staging.adopt(node.clone()).unwrap();
    }
    let (_, write) = stage_engine_state(
        &mut staging,
        &process,
        lash_vm_runtime::LASH_VM_ENGINE_KIND,
        &engine_state(NodeId(checkpoint.page.id.0), link_a, requirements_a.clone()),
        (None, None),
        CommitId::mint(),
    )
    .unwrap();
    let mut tx = store.begin(&actor, claim.epoch).await.unwrap();
    tx.stage_all(staging).write(write);
    store
        .commit(tx, CommitLabel::new("law.park"))
        .await
        .unwrap();
    let old = store.exec_root(&exec).await.unwrap().unwrap();
    let rewritten = rewritten(&document_a, &document_b, &a, &b);
    let declarations_only = registry(
        content
            .documents
            .values()
            .flat_map(|d| d.functions.values().cloned()),
        true,
    );
    let (page, fresh) = lash_vm_runtime::carry_parked_checkpoint(
        page_store.clone(),
        RootLeaseRequest {
            owner: RootOwner::Exec(exec.clone()),
            expected: old.root,
            fence: LeaseFence::Reader,
            ttl_millis: 600_000,
        },
        NodeId(checkpoint.page.id.0),
        rewritten,
        program(
            &content,
            link_b,
            declarations_only,
            PageId(closure_b.reference().0),
        ),
        bounds(),
        lash_kernel_migrate::CarryContext {
            kernel: 1,
            closure: PageId(closure_a.reference().0),
            links: content.links.clone(),
            selection: lash_kernel_doc::Selection::Fresh,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        store.exec_root(&exec).await.unwrap().unwrap(),
        old,
        "preparation publishes nothing"
    );
    let requirements_b =
        lash_vm_broker::kernel::loader::requirements(&closure_b, 1, std::iter::empty());
    assert_eq!(requirements_b.machine.natives, [b[2].identity()].into());
    assert!(requirements_a.machine.natives.is_empty());
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    for node in fresh.iter().chain(closure_b.nodes().values()) {
        staging.adopt(node.clone()).unwrap();
    }
    let (_, write) = stage_engine_state(
        &mut staging,
        &process,
        lash_vm_runtime::LASH_VM_ENGINE_KIND,
        &engine_state(page, link_b, requirements_b.clone()),
        (Some(old.rev), Some(old.root)),
        CommitId::mint(),
    )
    .unwrap();
    let mut tx = store.begin(&actor, claim.epoch).await.unwrap();
    tx.stage_all(staging).write(write);
    let tokens: Vec<String> =
        lash_vocabulary::claims::ObligationToken::for_requirements(&requirements_b)
            .map(String::from)
            .collect();
    tx.set_requires(tokens);
    // A refused domain write rolls back the new roots and the admission record.
    let mut rejected = tx.clone();
    rejected.write(DomainWrite::ExecRoot(
        lash_durable::domain::graph::ExecRootWrite::Retire {
            exec: exec.clone(),
            expected: NodeId([99; 32]),
        },
    ));
    assert!(
        store
            .commit(rejected, CommitLabel::new("law.refuse"))
            .await
            .is_err()
    );
    assert_eq!(store.exec_root(&exec).await.unwrap().unwrap(), old);
    tx.give_up(Release::Ready);
    store
        .commit(tx, CommitLabel::new("law.carry"))
        .await
        .unwrap();
    let old_node = store
        .register_node(&NodeSpec {
            node: lash_durable::NodeId::new("A-only"),
            decodes: vec![FormatSet::new("paged-carry-law")],
            serves: Vec::new(),
            capabilities: NodeCapabilities {
                machine: lash_vocabulary::claims::NodeRequirements {
                    kernels: [1].into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            ttl_millis: 600_000,
        })
        .await
        .unwrap();
    assert!(
        store.claim(&old_node, 1).await.unwrap().is_empty(),
        "A-only node must not claim B's native"
    );
    store.release_node(&old_node).await.unwrap();
    let new = store.exec_root(&exec).await.unwrap().unwrap();
    assert_ne!(new.root, old.root);
    let lease = page_store
        .acquire_root_lease(RootLeaseRequest {
            owner: RootOwner::Exec(exec),
            expected: new.root,
            fence: LeaseFence::Reader,
            ttl_millis: 600_000,
        })
        .await
        .unwrap();
    // A test oracle loads the stored graph, including all unchanged pages;
    // the production carry above read it page by page under its own lease.
    let mut pending = vec![new.root];
    let loaded = Pages::default();
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
        pending.extend(node.children().unwrap());
        loaded.0.borrow_mut().insert(id, node);
    }
    page_store.release_root_lease(lease.id).await.unwrap();
    let body: lash_core_store::checkpoint_graph::bodies::ExecRoot = {
        let held = loaded.0.borrow();
        let mut decoder = lash_core_store::checkpoint_graph::GraphDecoder::new(
            &*held,
            lash_core_store::checkpoint_graph::DecodeLimits::within_total(u64::MAX),
        );
        decoder
            .decode(
                new.root,
                lash_core_store::checkpoint_graph::NodeKind::EXEC_ROOT,
            )
            .unwrap()
    };
    assert_eq!(body.requirements, requirements_b);
    {
        let held = loaded.0.borrow();
        let mut decoder = lash_core_store::checkpoint_graph::GraphDecoder::new(
            &*held,
            lash_core_store::checkpoint_graph::DecodeLimits::within_total(u64::MAX),
        );
        let state: lash_core_store::checkpoint_graph::bodies::ProcessState = decoder
            .decode(
                body.state,
                lash_core_store::checkpoint_graph::NodeKind::PROCESS_STATE,
            )
            .unwrap();
        let lash_core_store::checkpoint_graph::bodies::ProcessStateBody::Kernel { root, bytes } =
            state.state
        else {
            panic!("kernel state");
        };
        let linked: lash_vm_runtime::KernelProcessInput = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(root, page);
        assert_eq!(
            linked.code,
            lash_vm_runtime::process::InlineRequest::Linked {
                start: LinkedStart {
                    link: link_b,
                    selector: StartSelector::Main
                }
            }
        );
    }
    assert!(loaded.0.borrow().contains_key(&closure_b.reference()));
    assert!(
        !loaded.0.borrow().contains_key(&closure_a.reference()),
        "the union closure supersedes the old closure node while retaining its definitions"
    );
    let answer = loaded
        .clone()
        .read(&[PageRef {
            id: PageId(page.0),
            grant: PageGrant::NONE,
        }])
        .unwrap()
        .remove(0)
        .unwrap();
    let checkpoint = lash_kernel_state::paged::Checkpoint::of_answer(&answer).unwrap();
    let mut resumed = KernelMachine::open(
        program(
            &content,
            link_b,
            b_registry,
            PageId(closure_b.reference().0),
        ),
        bounds(),
        &checkpoint,
        Box::new(loaded.clone()),
    )
    .unwrap();
    assert_eq!(
        finish(&mut resumed, request.wait, link_a, link_b),
        finish(&mut original, request.wait, link_a, link_a)
    );
    store.release_node(&node).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paged_carry_commits_one_consistent_side_and_resumes_after_retirement_on_sqlite() {
    Box::pin(atomic_carry(super::served::Tier::SqliteMemory)).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires PostgreSQL"]
async fn a_paged_carry_commits_one_consistent_side_and_resumes_after_retirement_on_postgres() {
    lash_postgres_store::testing::required_database_url();
    Box::pin(atomic_carry(super::served::Tier::Postgres)).await;
}
