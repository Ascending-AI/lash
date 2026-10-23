//! FIG-7153: Link-qualified carry preserves exact creation code and refuses foreign origins.
use super::paged_carry::*;
use lash::package::*;
use lash_core_store::checkpoint_graph::{EncodedNode, NodeId};
use lash_durable::domain::pages::{LeaseFence, RootLeaseRequest};
use lash_durable::domain::{ExecKey, RootOwner};
use lash_kernel_doc::heap::{PageGrant, PageId, PageRef};
use lash_kernel_doc::{Datum, Document};
use lash_kernel_doc::{Export, FunctionExport, StartConvention};
use lash_kernel_package::LinkedContent;
use lash_kernel_state::paged::PageSource;
use lash_kernel_vm::{KernelMachine, Machine};
use lash_vocabulary::names::Name;
use std::{collections::BTreeMap, sync::Arc};

struct Fixture {
    stores: Arc<dyn lash_execution_contracts::backend::StoreSet>,
    request: RootLeaseRequest,
    root: NodeId,
    pages: Pages,
    checkpoint: lash_kernel_vm::SealedRoot,
    content: LinkedContent,
    from: LinkId,
    to: LinkId,
    leaf: LinkId,
    closure: PageId,
    rewritten: lash_kernel_migrate::Rewritten,
    wait: lash_vocabulary::sites::WaitId,
}
impl Fixture {
    async fn new() -> Self {
        use lash_core_execution::runtime::actor::process::state_graph::stage_engine_state;
        use lash_core_store::checkpoint_graph::{
            CommitId, EncodeBudget, ExplicitLimit, GraphStaging,
        };
        use lash_durable::{ActorKey, CommitLabel, FormatSet, MailTx, NodeSpec};
        let stores: Arc<dyn lash_execution_contracts::backend::StoreSet> =
            Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
        let mut leaf = document(&release(false), false);
        // A retained closure's loop executes under its defining kernel's
        // cost table, even when the root's format has advanced.
        let local = *leaf
            .functions
            .keys()
            .find(|id| {
                !release(false)
                    .iter()
                    .any(|definition| definition.identity() == **id)
            })
            .unwrap();
        let mut definition = leaf.functions.remove(&local).unwrap();
        let loop_body = lash_kernel_doc::parse_document(
            "kernel 1\nnumbers float\nmain { for i in (1, 2) { do perform 0(7) as Any } }",
        )
        .unwrap()
        .main
        .unwrap();
        let lash_kernel_doc::Implementation::Guest { body, .. } = &mut definition.implementation
        else {
            unreachable!()
        };
        body.block
            .splice(body.block.len() - 1..body.block.len() - 1, loop_body);
        let next = definition.identity();
        leaf.functions.insert(next, definition);
        lash_kernel_migrate::replace_functions(&mut leaf, &[(local, next)].into());
        let body = leaf.main.take().unwrap();
        let mut entry = lash_kernel_doc::parse_definition(
            "function pkg.run() -> Any\nkernel 1\ncharge 1\nbody { return null }",
        )
        .unwrap();
        let lash_kernel_doc::Implementation::Guest {
            body: entry_body, ..
        } = &mut entry.implementation
        else {
            unreachable!()
        };
        entry_body.block = body;
        entry_body.functions = leaf
            .functions
            .iter()
            .map(|(id, f)| (*id, f.name.clone()))
            .collect();
        entry_body.imports = leaf.imports.clone();
        leaf.exports.insert(
            Name::new("run"),
            Export::Function(FunctionExport {
                function: entry.identity(),
                signature: entry.signature.clone(),
                convention: StartConvention::Direct,
                asynchronous: false,
                rest: false,
            }),
        );
        leaf.functions.insert(entry.identity(), entry);
        let (mut content, leaf) = publish(&stores, &leaf, "exact-A").await;
        let wrapper = lash_kernel_doc::parse_definition("function pkg.wrapper() -> Any\nkernel 1\nmetering statements\nimport 0 code pkg.run() -> Any direct\nbody { let answer = invoke import 0() return answer }").unwrap();
        let header = format!(
            "kernel 1\nnumbers float\nimport 0 code pkg.run() -> Any direct\ndefinition {}\n",
            serde_json::to_string(&lash_kernel_doc::print_definition(&wrapper)).unwrap()
        );
        let before = lash_kernel_doc::parse_document(&format!(
            "{header}main {{ let answer = invoke pkg.wrapper() return answer }}"
        ))
        .unwrap();
        let after = lash_kernel_doc::parse_document(&format!(
            "{header}main {{ let answer = invoke pkg.wrapper() let successor = 1 return answer }}"
        ))
        .unwrap();
        let mut add = |document: Document| {
            let link = Link {
                document: document.identity(),
                fills: [(
                    ImportSlot(0),
                    Fill::Export {
                        link: leaf,
                        export: ExportName {
                            name: Name::new("run"),
                            member: None,
                        },
                    },
                )]
                .into(),
            };
            let id = link.identity();
            content.documents.insert(document.identity(), document);
            content.links.insert(id, link);
            id
        };
        let from = add(before.clone());
        let to = add(after.clone());
        let functions = registry(
            content
                .documents
                .values()
                .flat_map(|d| d.functions.values().cloned()),
            false,
        );
        let closure = lash_vm_broker::kernel::closure::derive(
            content
                .documents
                .values()
                .flat_map(|d| d.functions.keys().copied()),
            &functions,
            &BTreeMap::new(),
            &lash_kernel_doc::BuildHelpers::empty(),
            &before.library,
        )
        .unwrap();
        let process = lash_core::ProcessIdMint::sequential_id_for_testing(7153);
        let exec = ExecKey::Process(process.clone());
        let actor = ActorKey::process(process.as_str()).unwrap();
        let store = stores.durable_store();
        let format = FormatSet::new("linked-carry-law");
        let mut mail = MailTx::new();
        mail.create_actor(actor.clone(), format.clone());
        store
            .commit_mail(mail, CommitLabel::new("law.create"))
            .await
            .unwrap();
        let node = store
            .register_node(&NodeSpec {
                node: lash_durable::NodeId::new("linked-writer"),
                decodes: vec![format],
                serves: Vec::new(),
                capabilities: Default::default(),
                ttl_millis: 600_000,
            })
            .await
            .unwrap();
        let claim = store.claim(&node, 1).await.unwrap().remove(0);
        let owner = lash_vm_broker::kernel::graph::owner_binding_node(&exec).unwrap();
        let pages = Pages::default();
        let (_, checkpoint, request) = park_with(
            program(&content, from, functions, PageId(closure.reference().0)),
            &pages,
            PageId(owner.0),
        );
        let state = lash_execution_contracts::process::EngineState {
            staged: Vec::new(),
            format: lash_execution_contracts::process::EngineStateFormat {
                kind: lash_vm_runtime::LASH_VM_ENGINE_KIND.into(),
                version: 1,
            },
            bytes: Vec::new(),
            pages: Some(NodeId(checkpoint.page.id.0)),
            requirements: Default::default(),
        };
        let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        for node in pages.0.borrow().values().chain(closure.nodes().values()) {
            staging.adopt(node.clone()).unwrap();
        }
        let (_, write) = stage_engine_state(
            &mut staging,
            &process,
            lash_vm_runtime::LASH_VM_ENGINE_KIND,
            &state,
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
        let root = store.exec_root(&exec).await.unwrap().unwrap().root;
        let rewritten = lash_kernel_migrate::Rewritten {
            correspondence: lash_kernel_migrate::unchanged(&before, &after).unwrap(),
            document: after,
            functions: BTreeMap::new(),
        };
        Self {
            stores,
            request: RootLeaseRequest {
                owner: RootOwner::Exec(exec),
                expected: root,
                fence: LeaseFence::Reader,
                ttl_millis: 600_000,
            },
            root,
            pages,
            checkpoint,
            content,
            from,
            to,
            leaf,
            closure: PageId(closure.reference().0),
            rewritten,
            wait: request.wait,
        }
    }
    fn context(&self) -> lash_kernel_migrate::CarryContext {
        lash_kernel_migrate::CarryContext {
            kernel: 1,
            closure: self.closure,
            code: self
                .content
                .inspect(
                    self.from,
                    &lash_kernel_package::ReachSelection::Start(StartSelector::Main),
                    lash_kernel_doc::BuildHelpers::empty().as_ref(),
                )
                .unwrap()
                .functions,
            links: self.content.links.clone(),
            selection: lash_kernel_doc::Selection::Fresh,
            retained: Default::default(),
            uses: self
                .content
                .links
                .iter()
                .flat_map(|(link, instance)| {
                    instance.fills.iter().filter_map(move |(slot, fill)| {
                        matches!(fill, Fill::Operation(_) | Fill::Deferred { .. }).then_some((
                            lash_vocabulary::OperationUse {
                                link: *link,
                                slot: *slot,
                            },
                            lash_vocabulary::OperationUse {
                                link: if *link == self.from { self.to } else { *link },
                                slot: *slot,
                            },
                        ))
                    })
                })
                .collect(),
        }
    }
    fn successor_program(&self) -> lash_kernel_vm::Program {
        let functions = registry(
            self.content
                .documents
                .values()
                .flat_map(|d| d.functions.values().cloned()),
            false,
        );
        let selected = self
            .content
            .inspect(
                self.to,
                &lash_kernel_package::ReachSelection::Start(StartSelector::Main),
                lash_kernel_doc::BuildHelpers::empty().as_ref(),
            )
            .unwrap();
        let document = &self.content.documents[&self.content.links[&self.to].document];
        let assembled = lash_vm_broker::kernel::loader::declare_reach(
            document,
            &self
                .content
                .documents
                .iter()
                .map(|(id, document)| (*id, Arc::new(document.clone())))
                .collect(),
            &selected.functions,
            &selected.machine,
            &functions,
            &lash_kernel_doc::BuildHelpers::empty(),
            &lash_kernel_doc::Selection::Fresh,
        )
        .unwrap();
        program(
            &self.content,
            self.to,
            functions,
            PageId(assembled.closure.reference().0),
        )
    }
    async fn carry(
        &self,
        rewritten: lash_kernel_migrate::Rewritten,
        context: lash_kernel_migrate::CarryContext,
    ) -> Result<(NodeId, Vec<EncodedNode>), lash_kernel_migrate::CarryFault> {
        lash_vm_runtime::carry_parked_checkpoint(
            self.stores.durable_store().pages().unwrap(),
            self.request.clone(),
            NodeId(self.checkpoint.page.id.0),
            rewritten,
            self.successor_program(),
            bounds(),
            context,
        )
        .await
    }
    async fn unchanged(&self) {
        let RootOwner::Exec(exec) = &self.request.owner else {
            unreachable!()
        };
        assert_eq!(
            self.stores
                .durable_store()
                .exec_root(exec)
                .await
                .unwrap()
                .unwrap()
                .root,
            self.root
        );
    }
    fn finish(&self, machine: &mut KernelMachine) -> lash_kernel_vm::Meters {
        machine
            .deliver(
                self.wait,
                lash_kernel_vm::Outcome::Completed(Datum::Int(7.into())),
            )
            .unwrap();
        for _ in 0..4 {
            let lash_kernel_vm::Step::Parked(park) = machine.run(&mut Host, u64::MAX).unwrap()
            else {
                panic!("the retained operation parks")
            };
            let [lash_kernel_vm::Request::Effect(request)] = park.requests.as_slice() else {
                panic!("one operation")
            };
            assert_eq!(
                request.use_,
                lash_vocabulary::OperationUse {
                    link: self.leaf,
                    slot: ImportSlot(0)
                }
            );
            machine
                .deliver(
                    request.wait,
                    lash_kernel_vm::Outcome::Completed(Datum::Int(7.into())),
                )
                .unwrap();
        }
        let lash_kernel_vm::Step::Ended(lash_kernel_vm::End::Finished(ended)) =
            machine.run(&mut Host, u64::MAX).unwrap()
        else {
            panic!("the carried run ends")
        };
        assert_eq!(
            ended.result,
            Datum::List(vec![Datum::Int(7.into()), Datum::Int(7.into())])
        );
        machine.meters()
    }
    async fn carried(&self) {
        let (root, nodes) = self
            .carry(self.rewritten.clone(), self.context())
            .await
            .expect("a proved foreign defining Link carries");
        self.unchanged().await;
        self.pages
            .0
            .borrow_mut()
            .extend(nodes.into_iter().map(|node| (node.id, node)));
        let answer = self
            .pages
            .clone()
            .read(&[PageRef {
                id: PageId(root.0),
                grant: PageGrant::NONE,
            }])
            .unwrap()
            .remove(0)
            .unwrap();
        let checkpoint = lash_kernel_state::paged::Checkpoint::of_answer(&answer).unwrap();
        let mut resumed = KernelMachine::open(
            self.successor_program(),
            bounds(),
            &checkpoint,
            Box::new(self.pages.clone()),
        )
        .unwrap();
        let carried = self.finish(&mut resumed);
        if self.rewritten.document.kernel() != 1 {
            let answer = self
                .pages
                .clone()
                .read(&[self.checkpoint.page])
                .unwrap()
                .remove(0)
                .unwrap();
            let original_checkpoint =
                lash_kernel_state::paged::Checkpoint::of_answer(&answer).unwrap();
            let mut original = KernelMachine::open(
                program(
                    &self.content,
                    self.from,
                    registry(
                        self.content
                            .documents
                            .values()
                            .flat_map(|d| d.functions.values().cloned()),
                        false,
                    ),
                    self.closure,
                ),
                bounds(),
                &original_checkpoint,
                Box::new(self.pages.clone()),
            )
            .unwrap();
            let prior = self.finish(&mut original);
            assert_eq!(
                carried.charged, prior.charged,
                "exact retained code keeps its defining kernel's loop charge"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_linked_library_spawn_and_captured_closure_resume_after_retirement_on_sqlite() {
    Fixture::new().await.carried().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unmapped_linked_spawn_origin_refuses_without_rewriting_on_sqlite() {
    let fixture = Fixture::new().await;
    fixture.carried().await;
    let mut rewritten = fixture.rewritten.clone();
    rewritten.correspondence = lash_kernel_doc::Correspondence::of(
        rewritten.correspondence.base,
        rewritten.correspondence.result,
        Vec::new(),
    )
    .unwrap();
    assert!(
        matches!(fixture.carry(rewritten, fixture.context()).await, Err(lash_kernel_migrate::CarryFault::Refused(lash_kernel_migrate::ParkedRefusal::SiteNotCarried { held, .. })) if &*held == "spawn origin")
    );
    fixture.unchanged().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the linked-format-carry synthetic-next feature witness"]
async fn a_linked_carry_keeps_exact_pinned_code_over_another_retained_release_on_sqlite() {
    let mut fixture = Fixture::new().await;
    let migration = lash_kernel_migrate::migration_from(lash_kernel_doc::KernelVersion::One)
        .expect("synthetic-next witness");
    let before = &fixture.content.documents[&fixture.content.links[&fixture.from].document];
    fixture.rewritten = (migration.document)(before, &before.functions).unwrap();
    let mut next = fixture.content.links[&fixture.from].clone();
    next.document = fixture.rewritten.document.identity();
    fixture.to = next.identity();
    fixture.content.links.insert(fixture.to, next);
    fixture.content.documents.insert(
        fixture.rewritten.document.identity(),
        fixture.rewritten.document.clone(),
    );
    let (other, _) = publish(
        &fixture.stores,
        &document(&release(true), true),
        "another-B",
    )
    .await;
    fixture.content.documents.extend(other.documents);
    fixture.content.links.extend(other.links);
    fixture.carried().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreign_linked_kernel_or_closure_refuses_before_rewriting_on_sqlite() {
    let fixture = Fixture::new().await;
    fixture.carried().await;
    for foreign_kernel in [true, false] {
        let mut context = fixture.context();
        if foreign_kernel {
            context.kernel = 99;
        } else {
            context.closure = PageId([99; 32]);
        }
        assert!(matches!(
            fixture.carry(fixture.rewritten.clone(), context).await,
            Err(lash_kernel_migrate::CarryFault::Refused(_))
        ));
        fixture.unchanged().await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absent_or_incompatible_defining_link_refuses_without_fallback_on_sqlite() {
    let fixture = Fixture::new().await;
    fixture.carried().await;
    let documents = lash_vm_runtime::KernelDocuments::new(
        fixture.stores.module_artifacts(),
        fixture.stores.package_store(),
        lash_kernel_doc::BuildHelpers::empty(),
    );
    assert!(
        lash_vm_runtime::carry_snapshot(&documents, LinkId::from_bytes([99; 32]))
            .await
            .is_err()
    );
    let mut context = fixture.context();
    context.links.remove(&fixture.leaf);
    assert!(
        fixture
            .carry(fixture.rewritten.clone(), context)
            .await
            .is_err()
    );
    let mut foreign = fixture.context();
    foreign.code.remove(&fixture.leaf);
    assert!(
        fixture
            .carry(fixture.rewritten.clone(), foreign)
            .await
            .is_err()
    );
    fixture.unchanged().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parked_callable_start_keeps_its_frozen_captures_across_a_release_on_sqlite() {
    use super::callback_carry::{Discovery, SERVICES, core};
    let stores: Arc<dyn lash_execution_contracts::backend::StoreSet> =
        Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
    let core = core(
        Arc::clone(&stores),
        "callable-carry",
        Arc::default(),
        Discovery::None,
    );
    let base = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(16))
        .component(SERVICES, &())
        .build()
        .unwrap();
    let pin = lash_core::HostArtifactPin::mint();
    let (packages, namespace) = super::kernel_process::namespace(&core, &pin, &base).await;
    let (payload, _) = super::callback_carry::publish(&core,
        "kernel 1\nnumbers float\nprivate captured\nimport 0 operation typescript.sleep(milliseconds: Int) -> Null\nmain { let captured = [7] let f = fn() presentation () -> Int bare direct { do perform 0(3000) as Null return captured[0] } return f }", &base, &pin, &packages, &namespace).await;
    let input: lash_vm_runtime::KernelProcessInput = serde_json::from_value(payload).unwrap();
    let (link, _) = input.code.linked().unwrap();
    let workers = lash_vm_client::service::Service::default();
    let image = workers.config().worker_library().unwrap();
    let documents = lash_vm_runtime::KernelDocuments::new(
        stores.module_artifacts(),
        stores.package_store(),
        Arc::clone(image.helpers()),
    );
    let content = documents.load_linked(link).await.unwrap();
    let document = &content.documents[&content.links[&link].document];
    let mut pending = vec![(
        lash_vocabulary::sites::Site::new(lash_vocabulary::sites::Unit::Main, []),
        lash_kernel_doc::Node::Block(document.main.as_ref().unwrap()),
    )];
    let (creation, callable, presentation) = loop {
        let (site, node) = pending.pop().unwrap();
        if let lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Closure(closure)) = node {
            break (
                site,
                closure.function,
                closure.presentation.clone().unwrap(),
            );
        }
        for (index, child) in (0u32..).zip(node.children()) {
            let mut at = site.clone();
            at.path.push(index);
            pending.push((at, child));
        }
    };
    let function = Box::new(lash_kernel_doc::FunctionValue {
        written_signature: None,
        signature_origin: None,
        implicit_text_conversions: Vec::new(),
        callable: lash_vocabulary::CallableId {
            link,
            function: callable,
        },
        captures: [(
            Name::new("captured"),
            lash_kernel_doc::CaptureValue::Data(Datum::List(vec![Datum::Int(7.into())])),
        )]
        .into(),
        signature: presentation.signature,
        convention: StartConvention::Direct,
        captions: lash_kernel_doc::Captions::empty(document.identity()),
        representation: lash_kernel_doc::FunctionRepresentation::Bare,
        creation: Some(creation),
        token: None,
    });
    let origin = CallableOrigin {
        authored: [link].into(),
        publications: packages
            .snapshot
            .advertisements
            .iter()
            .map(|accepted| accepted.publication.key())
            .collect(),
    };
    let definition = core
        .host_artifacts()
        .publish_callable(&pin, &function, &namespace, &origin, None)
        .await
        .unwrap();
    let env = core
        .host_artifacts()
        .link_process_env(
            &base,
            &lash::process::LinkTarget::Definition(definition.id.clone()),
            &packages,
            &Default::default(),
        )
        .await
        .unwrap();
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&pin, &env)
        .await
        .unwrap();
    let payload = serde_json::to_value(lash_vm_runtime::KernelProcessInput {
        code: lash_vm_runtime::process::InlineRequest::Callable {
            link,
            function,
            origin,
        },
        args: Default::default(),
    })
    .unwrap();
    let settings = core
        .processes()
        .engine_defaults(lash_vm_runtime::LASH_VM_ENGINE_KIND, &payload)
        .unwrap();
    let process = core
        .processes()
        .start(
            lash::process::ProcessStartRequest::engine(
                lash_vm_runtime::LASH_VM_ENGINE_KIND,
                payload.clone(),
                lash::process::EngineLaunch::new(env_ref, settings),
                lash::process::ProcessOriginator::host(),
                lash::process::LifetimeDecision::Detached,
            ),
            core.effect_host(),
        )
        .await
        .unwrap()
        .process_id;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let row = core.processes().get(&process).await.unwrap().unwrap();
            assert!(row.terminal().is_none(), "the callable parks: {row:?}");
            if !row.waits().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // A new host environment is a release change; creation code and captures stay exact.
    let next_base = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(17))
        .component(SERVICES, &())
        .build()
        .unwrap();
    let next = core
        .host_artifacts()
        .link_process_env(
            &next_base,
            &lash::process::LinkTarget::Definition(definition.id),
            &packages,
            &Default::default(),
        )
        .await
        .unwrap();
    let next_ref = core
        .host_artifacts()
        .publish_process_env(&pin, &next)
        .await
        .unwrap();
    assert_eq!(
        core.processes()
            .succeed(
                &process,
                lash::process::ProcessSuccessor::new(payload.clone(), next_ref.clone())
            )
            .await
            .unwrap(),
        lash::process::ProcessSuccession::Requested
    );
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let row = core.processes().get(&process).await.unwrap().unwrap();
            if let lash::process::ProcessInput::Engine {
                launch,
                payload: held,
                ..
            } = &row.input
                && launch.env == next_ref
            {
                assert_eq!(*held, payload);
                break;
            }
            assert!(
                row.terminal().is_none(),
                "succession commits before the timer settles"
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        core.processes().await_output(&process),
    )
    .await
    .unwrap()
    .unwrap();
    let lash::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the carried callable settles")
    };
    assert!(output.is_success(), "{output:?}");
    assert_eq!(output.value_for_projection(), serde_json::json!(7));
    core.shutdown().await.unwrap();
}
