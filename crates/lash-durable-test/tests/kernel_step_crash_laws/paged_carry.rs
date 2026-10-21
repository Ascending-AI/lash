#![allow(clippy::expect_used, clippy::unwrap_used)]
//! K-VER-005, K-LIB-009 and ORIGIN against actual parked page records.
use lash_core_store::checkpoint_graph::{EncodedNode, NodeId};
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_doc::{Datum, Document, FunctionDefinition, FunctionRegistry};
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
    fn clock(&mut self) -> lash_kernel_doc::Timestamp {
        panic!("no clock")
    }
    fn random(&mut self) -> u64 {
        panic!("no random")
    }
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
fn release_sources(next: bool) -> (String, Vec<FunctionDefinition>) {
    let native = "function pkg.extra() -> Any\nkernel 1\ncharge 1\nnative\n";
    let extra = lash_kernel_doc::parse_definition(native).unwrap();
    let called = if next {
        format!("use pkg.extra = @{}\n", extra.identity())
    } else {
        String::new()
    };
    let suffix = if next { "let unused = pkg.extra()" } else { "" };
    let fetch = format!(
        "function pkg.fetch(x: Any) -> Any\nkernel 1\ncharge 1\neffect pkg.echo(x?: Any) -> Any contract \"1\"\n{called}body {{ let v = perform pkg.echo(x) as Any {suffix} return v }}\n"
    );
    let fetch_definition = lash_kernel_doc::parse_definition(&fetch).unwrap();
    let start = format!(
        "function pkg.start(x: Any) -> Any\nkernel 1\ncharge 1\nuse pkg.fetch = @{}\nbody {{ let h = spawn invoke pkg.fetch(x) return h }}\n",
        fetch_definition.identity()
    );
    let start_definition = lash_kernel_doc::parse_definition(&start).unwrap();
    let mut definitions = vec![fetch_definition, start_definition];
    if next {
        definitions.push(extra);
    }
    (
        format!("{}{fetch}{start}", if next { native } else { "" }),
        definitions,
    )
}
fn release(next: bool) -> Vec<FunctionDefinition> {
    release_sources(next).1
}
fn document(definitions: &[FunctionDefinition]) -> Document {
    let uses = definitions
        .iter()
        .map(|definition| format!("use {} = @{}\n", definition.name, definition.identity()))
        .collect::<String>();
    lash_kernel_doc::parse_document(&format!(
        "kernel 1\nnumbers float\neffect pkg.echo(x?: Any) -> Any contract \"1\"\n{uses}main {{ let captured = [7] let f = fn() {{ return captured[0] }} let h = invoke pkg.start(7) let v = join h let saved = apply f() return [v, saved] }}"
    )).unwrap()
}
fn registry(definitions: &[FunctionDefinition]) -> Arc<FunctionRegistry> {
    let mut functions = FunctionRegistry::new();
    for definition in definitions
        .iter()
        .filter(|d| d.has_native())
        .chain(definitions.iter().filter(|d| !d.has_native()))
    {
        let native: Option<Arc<dyn lash_kernel_doc::NativeFunction>> = definition
            .has_native()
            .then(|| Arc::new(Extra) as Arc<dyn lash_kernel_doc::NativeFunction>);
        functions.register(definition.clone(), native).unwrap();
    }
    Arc::new(functions)
}
fn program(document: Document, functions: Arc<FunctionRegistry>, closure: PageId) -> Program {
    Program {
        document: Arc::new(document),
        library: lash_kernel_vm::PreparedLibrary::new(functions),
        closure,
    }
}
fn park_with(
    document: Document,
    functions: Arc<FunctionRegistry>,
    pages: &Pages,
    closure: PageId,
    owner: PageId,
) -> (
    KernelMachine,
    lash_kernel_vm::SealedRoot,
    lash_kernel_vm::EffectRequest,
) {
    let mut machine = KernelMachine::start(
        program(document, functions, closure),
        bounds(),
        lash_kernel_vm::CellStart {
            owner,
            target: lash_kernel_vm::Target::Main,
            args: Vec::new(),
            session: None,
            modes: lash_kernel_vm::EffectModes::recorded(),
            host: Vec::new(),
        },
        Box::new(pages.clone()),
    )
    .unwrap();
    let Step::Parked(park) = machine.run(&mut Host, u64::MAX).unwrap() else {
        panic!("park")
    };
    let lash_kernel_vm::Request::Effect(request) = park.requests[0].clone() else {
        panic!("effect")
    };
    let root = machine.checkpoint().unwrap();
    (machine, root, request)
}
fn finish(machine: &mut KernelMachine, wait: lash_vocabulary::sites::WaitId) -> Datum {
    machine
        .deliver(
            wait,
            lash_kernel_vm::Outcome::Completed(Datum::Int(7.into())),
        )
        .unwrap();
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
    let held = |next| {
        let source = release_sources(next).0;
        lash::package::HeldPackages::install([lash::package::Package::build(
            "pkg",
            if next { "B" } else { "A" },
            &lash::package::PackageSources {
                kernel: &source,
                surface: if next {
                    "internal operation echo(x?: Any) -> Any contract \"1\"\n"
                } else {
                    "operation echo(x?: Any) -> Any contract \"1\"\n"
                },
                labels: "",
            },
            &[],
        )
        .unwrap()])
        .unwrap()
    };
    let (held_a, held_b) = (held(false), held(true));
    for held in [&held_a, &held_b] {
        let (artifacts, sets) = lash::package::install_rows(held);
        store.install_packages(&artifacts, &sets).await.unwrap();
    }
    let authority = lash_vm_runtime::StoredAuthority::new(stores.clone());
    let declarations = b
        .iter()
        .map(|definition| (definition.identity(), Arc::new(definition.clone())))
        .collect();
    lash_vm_runtime::admit_internal(&authority, &declarations)
        .await
        .unwrap()
        .unwrap();
    // A same-named forged body has no B membership and cannot borrow B's authority.
    let forged = lash_kernel_doc::parse_definition("function pkg.forged(x: Any) -> Any\nkernel 1\ncharge 1\neffect pkg.echo(x?: Any) -> Any contract \"1\"\nbody { let v = perform pkg.echo(x) as Any return v }\n").unwrap();
    assert!(
        lash_vm_runtime::admit_internal(
            &authority,
            &BTreeMap::from([(forged.identity(), Arc::new(forged))])
        )
        .await
        .unwrap()
        .is_err()
    );
    let (a_registry, b_registry) = (registry(&a), registry(&b));
    let closure_a = lash_vm_broker::kernel::closure::derive(
        document(&a).manifest.functions.keys().copied(),
        &a_registry,
        &BTreeMap::new(),
    )
    .unwrap();
    let closure_b = lash_vm_broker::kernel::closure::derive(
        document(&b).manifest.functions.keys().copied(),
        &b_registry,
        &BTreeMap::new(),
    )
    .unwrap();
    let pages = Pages::default();
    let (mut original, checkpoint, request) = park_with(
        document(&a),
        a_registry,
        &pages,
        PageId(closure_a.reference().0),
        PageId(owner.0),
    );
    let engine_state = |page, document: &Document, packages, requirements| {
        lash_execution_contracts::process::EngineState {
            staged: Vec::new(),
            format: lash_execution_contracts::process::EngineStateFormat {
                kind: lash_vm_runtime::LASH_VM_ENGINE_KIND.into(),
                version: 1,
            },
            bytes: serde_json::to_vec(&lash_vm_runtime::KernelProcessInput {
                packages,
                declared_packages: held_a.surface().selection(),
                document: document.identity(),
                functions: document.manifest.functions.keys().copied().collect(),
                entry: lash_vocabulary::legacy_names::Name::new("run"),
                args: Default::default(),
            })
            .unwrap(),
            pages: Some(page),
            requirements,
        }
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
        &engine_state(
            NodeId(checkpoint.page.id.0),
            &document(&a),
            held_a.digest(),
            requirements_a.clone(),
        ),
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
    let mut both = a.clone();
    both.extend(b.clone());
    let rewritten = lash_kernel_migrate::adopt(
        &document(&a),
        &lash_kernel_migrate::relink(&a, &b),
        registry(&both).as_ref(),
    )
    .unwrap();
    let mut declarations_only = FunctionRegistry::declarations();
    for definition in b
        .iter()
        .filter(|d| d.has_native())
        .chain(b.iter().filter(|d| !d.has_native()))
    {
        declarations_only
            .register(definition.clone(), None)
            .unwrap();
    }
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
            document(&b),
            Arc::new(declarations_only),
            PageId(closure_b.reference().0),
        ),
        bounds(),
        (1, PageId(closure_a.reference().0)),
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
    assert_eq!(requirements_b.natives, [b[2].identity()].into());
    assert!(requirements_a.natives.is_empty());
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    for node in fresh.iter().chain(closure_b.nodes().values()) {
        staging.adopt(node.clone()).unwrap();
    }
    let (_, write) = stage_engine_state(
        &mut staging,
        &process,
        lash_vm_runtime::LASH_VM_ENGINE_KIND,
        &engine_state(page, &document(&b), held_b.digest(), requirements_b.clone()),
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
            capabilities: lash_kernel_doc::NodeCapabilities {
                kernels: [1].into(),
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
        assert_eq!(linked.packages, held_b.digest());
        assert_eq!(linked.document, document(&b).identity());
    }
    assert!(loaded.0.borrow().contains_key(&closure_b.reference()));
    assert!(
        !loaded.0.borrow().contains_key(&closure_a.reference()),
        "new root no longer reaches A's closure"
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
        program(document(&b), registry(&b), PageId(closure_b.reference().0)),
        bounds(),
        &checkpoint,
        Box::new(loaded.clone()),
    )
    .unwrap();
    assert_eq!(
        finish(&mut resumed, request.wait),
        finish(&mut original, request.wait)
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
