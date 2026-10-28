//! Full worker admission of genuine callback and escaped-helper checkpoints.
use super::*;
use lash_kernel_doc::heap::{HeapFault, PageGrant};
use lash_kernel_doc::{BuildHelpers, Document, FunctionRegistry, LibraryDependencies, Selection};
use lash_kernel_state::paged::{PageAnswer, PageBody};
use lash_vm_client::packages::WorkerLibrary;
use std::collections::BTreeMap;
use {
    lash_kernel_value::CallableId, lash_kernel_value::Fill, lash_kernel_value::FunctionId,
    lash_kernel_value::Link, lash_kernel_value::OperationRequirement,
};

const BOUNDS: Bounds = Bounds {
    memory_meter: lash_kernel_vm::MemoryMeter::On,
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 10_000_000,
    memory: 64 << 20,
    call_depth: 100,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

#[derive(Clone, Default)]
struct Pages(Arc<Mutex<PageRows>>);
#[derive(Default)]
struct PageRows {
    answers: BTreeMap<PageId, PageAnswer>,
    warm: Vec<page::ReadPage>,
}
fn capability(id: PageId) -> page::PageCapability {
    page::PageCapability {
        page: id.0,
        authority: page::PageAuthority::Root(0),
        epoch: 0,
        mac: [0; 32],
    }
}
impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        let rows = self.0.lock().unwrap();
        Ok(pages
            .iter()
            .map(|page| {
                rows.answers
                    .get(&page.id)
                    .cloned()
                    .map(|mut answer| {
                        answer.page = *page;
                        answer
                    })
                    .ok_or(HeapFault::Missing { page: page.id })
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let framed = lash_vm_broker::kernel::pages::frame_page(&body)?;
        let children: Vec<_> = framed
            .children()
            .iter()
            .map(|id| capability(PageId(id.0)))
            .collect();
        let envelope = page::PageEnvelope::new(framed.into_bytes());
        let page = PageRef {
            id: PageId(envelope.id()),
            grant: PageGrant::NONE,
        };
        let answer = lash_vm_broker::kernel::pages::answer_crossed(page, &envelope, |_| {
            Ok(PageGrant::NONE)
        })?;
        let mut rows = self.0.lock().unwrap();
        rows.warm.push(page::ReadPage {
            page: envelope,
            children,
        });
        rows.answers.insert(page.id, answer);
        Ok(page)
    }
}
struct World;
impl lash_kernel_vm::Host for World {
    fn print(&mut self, _: &Datum) {
        panic!("no prints")
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}
struct Fixture {
    document: Arc<Document>,
    link: Link,
    registry: FunctionRegistry,
    helpers: Arc<BuildHelpers>,
    ids: BTreeMap<&'static str, FunctionId>,
    pages: Pages,
    checkpoint: Checkpoint,
    root: PageRef,
    wait: lash_kernel_vm::WaitId,
}
fn fixture() -> Fixture {
    // Worker bindings require the build's exact declarations for generated plans.
    let mut registry = lash_kernel_releases::helper_release()
        .unwrap()
        .functions
        .clone();
    let mut ids = BTreeMap::new();
    let mut definitions = String::new();
    for (name, text) in [
        ("child", "function law.child(x: Any) -> Any\nkernel 1\ncharge 1\nbody { return x }".to_owned()),
        ("saved", "function law.saved() -> Any\nkernel 1\ncharge 1\nbody { return 9 }".to_owned()),
        ("dead", "function law.dead() -> Any\nkernel 1\ncharge 1\nbody { return 1 }".to_owned()),
        ("twice", "function law.twice(f: Fn(x: Any) -> Any, x: Any) -> Any\nkernel 1\ncharge 5\nimport 4294901762 operation typescript.sleep(milliseconds: Int) -> Null\nbody { let a = apply f(x) let b = apply f(a) return b }".to_owned()),
    ] {
        let definition = lash_kernel_doc::parse_definition(&text).unwrap();
        definitions.push_str(&format!("definition {}\n", serde_json::to_string(&text).unwrap()));
        ids.insert(name, registry.register(definition, None).unwrap());
    }
    let parent = format!(
        "function law.parent() -> Fn(x: Any) -> Any\nkernel 1\ncharge 1\nuse law.child = @{}\nbody {{ return &law.child }}",
        ids["child"]
    );
    let definition = lash_kernel_doc::parse_definition(&parent).unwrap();
    ids.insert("parent", registry.register(definition, None).unwrap());
    definitions.push_str(&format!(
        "definition {}\n",
        serde_json::to_string(&parent).unwrap()
    ));
    let callback = "function law.callback(x: Any) -> Any\nkernel 1\nmetering statements\nimport 0 operation host.echo(x: Any) -> Any\nbody { let answer = perform 0(x) as Any return answer }";
    definitions.push_str(&format!(
        "definition {}\n",
        serde_json::to_string(callback).unwrap()
    ));
    let helpers = Arc::new(BuildHelpers::new(registry.clone(), ids.values().copied()).unwrap());
    // Park under the installed image, including both versions in synthetic-next,
    // so equal-image restore compares the same helper membership.
    let image = WorkerLibrary::with_helpers(
        &registry,
        helpers.members().copied(),
        &Default::default(),
        Arc::clone(&helpers),
    )
    .unwrap();
    let registry = image.registry().clone();
    let helpers = Arc::clone(image.helpers());
    let mut document = lash_kernel_doc::parse_document(&format!(
        "kernel 1\nnumbers by_spelling\nimport 0 operation host.echo(x: Any) -> Any\n{definitions}main {{ let unused = invoke law.dead() let child = invoke law.parent() let saved = &law.saved let callback = &law.callback let value = invoke law.twice(callback, 7) let answer = apply child(value) let retained = saved return answer }}"
    )).unwrap();
    document.functions.retain(|id, _| !helpers.contains(id));
    document.library =
        LibraryDependencies([ids["dead"], ids["parent"], ids["saved"], ids["twice"]].into());
    let fills = document
        .imports
        .iter()
        .map(|(slot, import)| {
            let lash_kernel_value::ImportKind::Operation { signature } = &import.kind else {
                panic!("operation")
            };
            (
                *slot,
                Fill::Operation(OperationRequirement::new(
                    signature.clone(),
                    lash_kernel_value::ContractId::new("1"),
                )),
            )
        })
        .collect();
    let link = Link {
        helper_fills: lash_kernel_doc::helper_imports(&document, helpers.as_ref())
            .unwrap()
            .into_iter()
            .map(|(id, import)| {
                let lash_kernel_value::ImportKind::Operation { signature } = &import.kind else {
                    panic!("helper operation")
                };
                (
                    id,
                    lash_kernel_value::HelperFill {
                        fill: Fill::Operation(OperationRequirement::new(
                            signature.clone(),
                            lash_kernel_value::ContractId::new("1"),
                        )),
                        import,
                    },
                )
            })
            .collect(),
        document: document.identity(),
        fills,
    };
    let document = Arc::new(document);
    let closure = lash_vm_broker::kernel::closure::derive(
        document.functions.keys().copied(),
        &registry,
        &document
            .functions
            .iter()
            .map(|(id, definition)| (*id, Arc::new(definition.clone())))
            .collect(),
        &helpers,
        &document.library,
    )
    .unwrap();
    let program = Program::prepare(
        link.identity(),
        [(document.identity(), Arc::clone(&document))].into(),
        &[(link.identity(), link.clone())].into(),
        PageId(closure.reference().0),
        lash_kernel_vm::PreparedLibrary::with_helpers(
            Arc::new(registry.clone()),
            Arc::clone(&helpers),
        ),
        Selection::Fresh,
    )
    .unwrap();
    let pages = Pages::default();
    let mut machine = KernelMachine::start(
        program,
        BOUNDS,
        CellStart {
            owner: PageId([9; 32]),
            target: lash_kernel_vm::Target::Main,
            args: vec![],
            session: None,
            modes: Default::default(),
            host: vec![],
        },
        Box::new(pages.clone()),
    )
    .unwrap();
    let Step::Parked(park) = machine.run(&mut World, u64::MAX).unwrap() else {
        panic!("callback parks")
    };
    let lash_kernel_vm::Request::Effect(effect) = &park.requests[0];
    let wait = effect.wait;
    let sealed = machine.checkpoint().unwrap();
    let root = sealed.page;
    let checkpoint = sealed.checkpoint();
    Fixture {
        document,
        link,
        registry,
        helpers,
        ids,
        pages,
        checkpoint,
        root,
        wait,
    }
}
impl Fixture {
    fn image(&self, removed: &[&str], extra: bool) -> WorkerLibrary {
        let mut registry = self.registry.clone();
        let mut roots: Vec<_> = self
            .ids
            .iter()
            .filter(|(name, _)| !removed.contains(name))
            .map(|(_, id)| *id)
            .collect();
        if extra {
            let definition = lash_kernel_doc::parse_definition(
                "function law.unrelated() -> Any\nkernel 1\ncharge 1\nbody { return 42 }",
            )
            .unwrap();
            roots.push(registry.register(definition, None).unwrap());
        }
        if removed.contains(&"child") {
            let replacement = lash_kernel_doc::parse_definition(
                "function law.child(x: Any) -> Any\nkernel 1\ncharge 1\nbody { return 99 }",
            )
            .unwrap();
            let child = registry.register(replacement, None).unwrap();
            assert_ne!(child, self.ids["child"]);
            let replacement = lash_kernel_doc::parse_definition(&format!("function law.parent() -> Fn(x: Any) -> Any\nkernel 1\ncharge 1\nuse law.child = @{child}\nbody {{ return &law.child }}")).unwrap();
            let parent = registry.register(replacement, None).unwrap();
            assert_ne!(parent, self.ids["parent"]);
            roots.extend([child, parent]);
        }
        let helpers = Arc::new(BuildHelpers::new(registry.clone(), roots).unwrap());
        // Keep the worker's build declarations and selected law helpers only;
        // removed law helpers must be absent, not merely unreachable.
        let mut selected = lash_kernel_releases::helper_release()
            .unwrap()
            .functions
            .clone();
        for name in ["child", "saved", "dead", "twice", "parent"] {
            let id = self.ids[name];
            if helpers.contains(&id) {
                selected
                    .register(
                        self.registry
                            .get(&id)
                            .unwrap()
                            .definition()
                            .as_ref()
                            .clone(),
                        None,
                    )
                    .unwrap();
            }
        }
        let mut pending: std::collections::BTreeSet<_> = helpers
            .members()
            .filter(|id| selected.get(id).is_none())
            .copied()
            .collect();
        while !pending.is_empty() {
            let id = *pending
                .iter()
                .find(|id| {
                    registry
                        .get(id)
                        .unwrap()
                        .callees()
                        .iter()
                        .all(|callee| selected.get(callee).is_some())
                })
                .unwrap();
            selected
                .register(
                    registry.get(&id).unwrap().definition().as_ref().clone(),
                    None,
                )
                .unwrap();
            pending.remove(&id);
        }
        WorkerLibrary::with_helpers(
            &selected,
            helpers.members().copied(),
            &Default::default(),
            Arc::clone(&helpers),
        )
        .unwrap()
    }
    fn selection(&self, image: &WorkerLibrary) -> Result<Selection, PoolError> {
        self.checkpoint
            .check_helper_compatibility(
                Box::new(self.pages.clone()),
                &[(self.link.identity(), self.document.as_ref().clone())].into(),
                &Default::default(),
                image.helpers(),
            )
            .map(|(_, selection)| selection)
            .map_err(helper_compatibility_refusal)
    }
    fn start(&self, selection: &Selection) -> Start {
        let mut warm = self.pages.0.lock().unwrap().warm.clone();
        warm.reverse();
        Start {
            owner: VmOwner::new("helper-restore-law"),
            dialect: "typescript".into(),
            document: message::StoredDocument::supplied(EncodedPayload(
                self.document.canonical_bytes(),
            )),
            link: self.link.identity(),
            instances: wire::encode(
                PayloadKind::Start,
                &vec![lash_kernel_vm::PreparedInstance {
                    helper_operations: self
                        .link
                        .helper_fills
                        .iter()
                        .map(|(id, binding)| (*id, binding.import.clone()))
                        .collect(),
                    link: self.link.identity(),
                    document: self.document.identity(),
                    slots: self
                        .link
                        .fills
                        .iter()
                        .map(|(slot, fill)| {
                            let Fill::Operation(_) = fill else {
                                panic!("operation")
                            };
                            (
                                *slot,
                                lash_kernel_vm::PreparedSlot::Operation(
                                    lash_kernel_value::OperationUse {
                                        link: self.link.identity(),
                                        slot: (*slot).into(),
                                    },
                                ),
                            )
                        })
                        .collect(),
                }],
            )
            .unwrap(),
            documents: vec![message::StoredDocument::supplied(EncodedPayload(
                self.document.canonical_bytes(),
            ))],
            functions: self.document.functions.keys().copied().collect(),
            definitions: vec![],
            from: StartFrom::Checkpoint(message::CheckpointStart {
                root: capability(self.root.id),
                admissions: vec![],
                selection: match selection {
                    Selection::Fresh => None,
                    Selection::Restored(restored) => {
                        Some(wire::encode(PayloadKind::Start, restored).unwrap())
                    }
                },
            }),
            bounds: lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper(
                "unused",
            ))
            .run_bounds,
            binding: [9; 32],
            save: None,
            warm,
            observe_sites: false,
            observe_counts: false,
            observe_path: false,
            memory_limiter: false,
            charge_limiter: false,
        }
    }
}
fn embed(image: &WorkerLibrary) -> Embedding {
    crate::embedding::Embedder::new()
        .dialect(crate::typescript(&lash_vm_client::WorkerTuning::standard()).unwrap())
        .unwrap()
        .finish(image)
        .unwrap()
}
fn resume(fixture: &Fixture, embedding: &Embedding, selection: &Selection) -> Hosted {
    let config =
        lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"));
    let (pipe, _parent) = UnixStream::pair().unwrap();
    let mut server = Server::new(
        pipe,
        FrameCodec::new(config.protocol.decode),
        Bootstrap::from(&config),
        embedding,
    )
    .unwrap();
    server.start(fixture.start(selection)).unwrap()
}
fn finish(mut hosted: Hosted, wait: lash_kernel_vm::WaitId) {
    hosted
        .machine
        .deliver(
            wait,
            lash_kernel_vm::Outcome::Completed(Datum::Int(7.into())),
        )
        .unwrap();
    loop {
        match hosted.machine.run(&mut World, u64::MAX).unwrap() {
            Step::Slice => {}
            Step::Parked(park) => {
                for request in park.requests {
                    let lash_kernel_vm::Request::Effect(effect) = request;
                    hosted
                        .machine
                        .deliver(
                            effect.wait,
                            lash_kernel_vm::Outcome::Completed(Datum::Int(7.into())),
                        )
                        .unwrap();
                }
            }
            Step::Ended(lash_kernel_vm::End::Finished(done)) => {
                assert_eq!(done.result, Datum::Int(7.into()));
                break;
            }
            other => panic!("resume result: {other:?}"),
        }
    }
}
/// Equal-image worker admission, both with a cold and already used embedding.
#[test]
fn equal_helper_library_restore_does_no_compatibility_work() {
    use lash_kernel_state::paged::helper_compatibility_work as probe;
    let fixture = fixture();
    let image = fixture.image(&[], false);
    assert_eq!(fixture.helpers.identity(), image.helpers().identity());
    let embedding = embed(&image);
    for cold in [false, true] {
        let cold_embedding;
        let embedding = if cold {
            cold_embedding = embed(&image);
            &cold_embedding
        } else {
            &embedding
        };
        probe::enable();
        let _ = lash_kernel_doc::document_work::helper_work();
        let selection = fixture.selection(&image).unwrap();
        assert_eq!(
            lash_kernel_doc::document_work::helper_work(),
            lash_kernel_doc::document_work::HelperWork::default()
        );
        let hosted = resume(&fixture, embedding, &selection);
        let work = probe::take().unwrap();
        assert_eq!(
            (
                work.live_code_walks,
                work.membership_probes,
                work.dependency_steps
            ),
            (0, 0, 0)
        );
        finish(hosted, fixture.wait);
    }
}
/// Active and saved not-yet-called helpers refuse before worker preparation.
#[test]
fn a_helper_absent_from_the_local_universe_is_refused_before_resume() {
    let fixture = fixture();
    let valid = fixture
        .selection(&fixture.image(&["parent", "dead"], true))
        .unwrap();
    for name in ["twice", "saved", "child"] {
        let image = fixture.image(&["parent", "dead", name], true);
        let error = fixture.selection(&image).unwrap_err();
        let missing = |outcome| matches!(outcome, InfrastructureOutcome::RunRefused { refusal: RunRefusal::Restore { refusal: KernelRestoreRefusal::HelperUnavailable { reason: lash_kernel_value::HelperUnavailableReason::Unknown, function } } } if function == fixture.ids[name]);
        assert!(missing(error.into_outcome()));
        let embedding = embed(&image);
        let config =
            lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"));
        let (pipe, _parent) = UnixStream::pair().unwrap();
        let mut server = Server::new(
            pipe,
            FrameCodec::new(config.protocol.decode),
            Bootstrap::from(&config),
            &embedding,
        )
        .unwrap();
        let _ = lash_kernel_doc::document_work::helper_work();
        let error = server
            .start(fixture.start(&valid))
            .err()
            .expect("worker refuses before preparing the helper bodies");
        assert!(missing(error.into_outcome()));
        assert_eq!(
            lash_kernel_doc::document_work::helper_work(),
            lash_kernel_doc::document_work::HelperWork::default()
        );
    }
}
/// A live returned child survives removal of its parent and a dead helper;
/// its exact identity is the pin, not current-manifest ancestry.
#[test]
fn an_unchanged_pinned_helper_survives_an_unrelated_library_change() {
    use lash_kernel_state::paged::helper_compatibility_work as probe;
    let fixture = fixture();
    let image = fixture.image(&["parent", "dead"], true);
    probe::enable();
    let _ = lash_kernel_doc::document_work::helper_work();
    let selection = fixture.selection(&image).unwrap();
    assert_eq!(
        lash_kernel_doc::document_work::helper_work(),
        lash_kernel_doc::document_work::HelperWork::default()
    );
    let work = probe::take().unwrap();
    let Selection::Restored(restored) = &selection else {
        panic!("changed image")
    };
    assert_eq!(work.live_code_walks, 1);
    assert_eq!(work.membership_probes as usize, restored.pins.len());
    assert!(restored.live.contains(&CallableId {
        link: fixture.link.identity(),
        function: fixture.ids["child"]
    }));
    for name in ["parent", "dead"] {
        assert!(!restored.pins.contains(&fixture.ids[name]));
    }
    finish(resume(&fixture, &embed(&image), &selection), fixture.wait);
}

/// A callback helper carries under an unchanged service contract. A changed
/// helper fill refuses before rewriting, and the original checkpoint still runs.
#[test]
fn a_callback_helper_carries_only_with_its_original_operation_contract() {
    let fixture = fixture();
    let image = fixture.image(&["parent", "dead"], true);
    let selection = fixture.selection(&image).unwrap();
    let Selection::Restored(prior) = &selection else {
        panic!("changed image")
    };
    let mut document = fixture.document.as_ref().clone();
    document
        .private_bindings
        .insert(lash_kernel_value::Name::new("carry_marker"));
    let rewritten = lash_kernel_migrate::Rewritten {
        correspondence: lash_kernel_migrate::unchanged(&fixture.document, &document).unwrap(),
        document: document.clone(),
        functions: Default::default(),
    };
    for changed in [true, false] {
        let mut next = Link {
            helper_fills: fixture.link.helper_fills.clone(),
            document: document.identity(),
            fills: fixture.link.fills.clone(),
        };
        if changed {
            let Fill::Operation(requirement) =
                &mut next.helper_fills.values_mut().next().unwrap().fill
            else {
                panic!("operation")
            };
            requirement.contract = lash_kernel_value::ContractId::new("changed");
        }
        let links: BTreeMap<_, _> = [
            (fixture.link.identity(), fixture.link.clone()),
            (next.identity(), next.clone()),
        ]
        .into();
        let mut carried = prior.clone();
        carried.live.extend(
            prior
                .live
                .iter()
                .filter(|callable| callable.link == fixture.link.identity())
                .map(|callable| CallableId {
                    link: next.identity(),
                    function: callable.function,
                }),
        );
        let next_selection = Selection::Restored(carried);
        let program = Program::prepare(
            next.identity(),
            [
                (fixture.document.identity(), Arc::clone(&fixture.document)),
                (document.identity(), Arc::new(document.clone())),
            ]
            .into(),
            &links,
            fixture.checkpoint.header().closure,
            lash_kernel_vm::PreparedLibrary::with_helpers(
                Arc::clone(embed(&image).registry().unwrap()),
                Arc::clone(image.helpers()),
            ),
            next_selection,
        )
        .unwrap();
        let context = lash_kernel_migrate::CarryContext {
            kernel: 1,
            closure: fixture.checkpoint.header().closure,
            code: [(
                fixture.link.identity(),
                fixture.document.functions.keys().copied().collect(),
            )]
            .into(),
            links,
            selection: selection.clone(),
            retained: lash_kernel_state::paged::checkpoint_code(
                &fixture.checkpoint,
                Box::new(fixture.pages.clone()),
            )
            .unwrap()
            .callables,
            uses: BTreeMap::new(),
        };
        let mut source = fixture.pages.clone();
        let carried = lash_kernel_migrate::carry_checkpoint(
            &mut source,
            &fixture.checkpoint,
            &rewritten,
            program.closure,
            next.identity(),
            &context,
            image.helpers(),
        );
        if changed {
            assert!(
                matches!(carried, Err(lash_kernel_migrate::CarryFault::Refused(lash_kernel_migrate::ParkedRefusal::HelperOperationContext { function, slot })) if function == fixture.ids["twice"] && slot == lash_kernel_value::ImportSlot(0xffff0002))
            );
            continue;
        }
        let (_, checkpoint) = carried.unwrap();
        assert_eq!(
            checkpoint.header().helper_library,
            image.helpers().identity()
        );
        let machine = KernelMachine::open(program, BOUNDS, &checkpoint, Box::new(source)).unwrap();
        let (pipe, _parent) = UnixStream::pair().unwrap();
        let config =
            lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"));
        let embedding = embed(&image);
        let mut server = Server::new(
            pipe,
            FrameCodec::new(config.protocol.decode),
            Bootstrap::from(&config),
            &embedding,
        )
        .unwrap();
        let port =
            WirePageSource::new(server.wire().unwrap(), config.tuning.pages.clean_bytes).port();
        finish(
            Hosted {
                machine,
                memory: BOUNDS.memory,
                ended: false,
                sealed: None,
                port,
                observe_sites: false,
                path: None,
            },
            fixture.wait,
        );
    }
    finish(resume(&fixture, &embed(&image), &selection), fixture.wait);
}

/// K-CALLABLE-RESTORE: fresh callable admission refuses a removed helper,
/// while the original parked run retains its compatible restore selection.
#[test]
fn fresh_callable_admission_cannot_grant_removed_helper_authority() {
    let fixture = fixture();
    let image = fixture.image(&["parent", "dead"], true);
    let selection = fixture.selection(&image).unwrap();
    let mut document = Document::new(lash_kernel_doc::NumberPolicy::BySpelling, vec![]);
    document.main = None;
    document.library = LibraryDependencies([fixture.ids["dead"]].into());
    let body = lash_kernel_doc::parse_definition(&format!(
        "function local.function() -> Json\nkernel 1\nmetering statements\nuse law.dead = @{}\nbody {{ return invoke law.dead() }}", fixture.ids["dead"]
    )).unwrap();
    let function = body.identity();
    document.functions.insert(function, body);
    let mut callable = lash_kernel_doc::FunctionValue {
        callable: CallableId {
            link: fixture.link.identity(),
            function,
        },
        captures: [(
            lash_kernel_value::Name::new("recursive_surplus"),
            lash_kernel_doc::CaptureValue::Reference(0),
        )]
        .into(),
        signature: lash_kernel_value::Signature {
            params: vec![],
            result: lash_kernel_value::Type::Json,
        },
        convention: lash_kernel_doc::StartConvention::Direct,
        representation: lash_kernel_doc::FunctionRepresentation::Bare,
        captions: lash_kernel_doc::Captions::empty(document.identity()),
        creation: None,
        token: None,
        signature_origin: None,
        written_signature: None,
        implicit_text_conversions: vec![],
    };
    assert!(
        callable
            .entry_export(&document, fixture.helpers.as_ref(), &Selection::Fresh)
            .is_ok()
    );

    let link = lash_kernel_value::Link {
        helper_fills: Default::default(),
        document: document.identity(),
        fills: Default::default(),
    };
    callable.callable.link = link.identity();
    let content = lash_kernel_package::LinkedContent {
        documents: [(document.identity(), document.into())].into(),
        links: [(link.identity(), link)].into(),
        ..Default::default()
    };
    let opened = lash_kernel_package::materialize_workflow(
        &content,
        &lash_kernel_doc::WorkflowReference::Callable(callable.callable),
        Some(&callable),
        None,
        fixture.helpers.as_ref(),
    )
    .unwrap();
    let document = opened.document;
    let error = lash_kernel_check::WorkflowDocument::entered(
        document.clone(),
        lash_kernel_value::LinkedStart {
            link: callable.callable.link,
            selector: opened.selector,
        },
        image.helpers().as_ref(),
    )
    .unwrap_err();
    let lash_kernel_check::WorkflowDocumentError::Refused(refused) = error else {
        panic!("fresh admission refuses unavailable helper identity: {error}");
    };
    assert!(refused.errors.iter().any(|error| matches!(&error.reason, lash_kernel_check::RefusalReason::Invalid(lash_kernel_doc::InvalidReason::HelperUnavailable { function, reason: lash_kernel_value::HelperUnavailableReason::Unknown }) if *function == fixture.ids["dead"])), "{refused:#?}");
    assert!(!document.functions.contains_key(&fixture.ids["dead"]));
    let embedding = embed(&image);
    finish(resume(&fixture, &embedding, &selection), fixture.wait);
}

/// FIG-7523 law 4: a drained helper leaves an anonymous iterator next in saved state.
#[test]
fn an_iterator_next_closure_refuses_upgrade_and_completes_on_its_old_body() {
    let old_text = "function law.iterator() -> Any\nkernel 1\ncharge 1\nbody { let next = fn() { return 7 } return {next: next} }";
    let new_text = old_text.replace("return 7", "return 99");
    let (owner, locals) = lash_kernel_doc::parse_definition_with_locals(old_text).unwrap();
    let (next_owner, next_locals) =
        lash_kernel_doc::parse_definition_with_locals(&new_text).unwrap();
    let old_next = locals[0].identity();
    assert_ne!(old_next, next_locals[0].identity());
    let old_owner = owner.identity();
    let current_owner = next_owner.identity();
    let current_next = next_locals[0].identity();
    let mut registry = FunctionRegistry::declarations();
    for definition in locals
        .into_iter()
        .chain([owner])
        .chain(next_locals)
        .chain([next_owner])
    {
        registry.register(definition, None).unwrap();
    }
    let helpers = Arc::new(BuildHelpers::new(registry.clone(), [old_owner]).unwrap());
    let release = |root: FunctionId, child: FunctionId| {
        use lash_kernel_releases::archive::{Binding, HelperId, Library, Release};
        let definitions: BTreeMap<_, _> = [root, child]
            .into_iter()
            .map(|id| (id, registry.get(&id).unwrap().definition().as_ref().clone()))
            .collect();
        let contract =
            lash_kernel_releases::archive::contract(root, &definitions, &BTreeMap::new()).unwrap();
        let library = Library {
            identity: lash_kernel_value::HelperLibraryId::of_members([root, child]),
            members: [root, child].into(),
            roots: [root].into(),
        };
        Release {
            library: library.clone(),
            definitions,
            libraries: [("fixture".into(), library)].into(),
            bindings: [(
                HelperId("fixture:law.iterator".into()),
                Binding {
                    revision: contract.revision(),
                    function: root,
                },
            )]
            .into(),
            ..Default::default()
        }
    };
    let archive = lash_kernel_releases::archive::Archive::default()
        .append(release(old_owner, old_next))
        .unwrap()
        .append(release(current_owner, current_next))
        .unwrap();
    let upgrade = archive.upgrade(&[old_owner].into()).unwrap();

    for branded in [false, true] {
        let brand = if branded {
            ", __lash_brand: (\"fixture\", \"iterator\")"
        } else {
            ""
        };
        let mut document = lash_kernel_doc::parse_document(&format!(
        "kernel 1\nnumbers by_spelling\nimport 0 operation host.pause() -> Any\nlibrary @{old_owner}\nmain {{ let stamp = {{__lash_built: b\"{old_owner}\"{brand}}} let iterator = invoke @{old_owner}() let next = iterator.next let pause = perform 0() as Any let answer = apply next() return answer }}",
        )).unwrap();
        document.functions.retain(|id, _| !helpers.contains(id));
        document.library = LibraryDependencies([old_owner].into());
        let signature = match &document.imports[&lash_kernel_value::ImportSlot(0)].kind {
            lash_kernel_value::ImportKind::Operation { signature } => signature.clone(),
            _ => unreachable!(),
        };
        let link = Link {
            document: document.identity(),
            helper_fills: Default::default(),
            fills: [(
                lash_kernel_value::ImportSlot(0),
                Fill::Operation(OperationRequirement::new(
                    signature,
                    lash_kernel_value::ContractId::new("1"),
                )),
            )]
            .into(),
        };
        let document = Arc::new(document);
        let program = || {
            Program::prepare(
                link.identity(),
                [(document.identity(), Arc::clone(&document))].into(),
                &[(link.identity(), link.clone())].into(),
                PageId([8; 32]),
                lash_kernel_vm::PreparedLibrary::with_helpers(
                    Arc::new(registry.clone()),
                    Arc::clone(&helpers),
                ),
                Selection::Fresh,
            )
            .unwrap()
        };
        let pages = Pages::default();
        let mut machine = KernelMachine::start(
            program(),
            BOUNDS,
            CellStart {
                owner: PageId([9; 32]),
                target: lash_kernel_vm::Target::Main,
                args: vec![],
                session: None,
                modes: Default::default(),
                host: vec![],
            },
            Box::new(pages.clone()),
        )
        .unwrap();
        let Step::Parked(park) = machine.run(&mut World, u64::MAX).unwrap() else {
            panic!("iterator is retained at a genuine park")
        };
        let lash_kernel_vm::Request::Effect(effect) = &park.requests[0];
        let checkpoint = machine.checkpoint().unwrap().checkpoint();
        drop(machine);
        let refused = checkpoint.check_helper_upgrade(
            Box::new(pages.clone()),
            &[(link.identity(), document.as_ref().clone())].into(),
            &Default::default(),
            &helpers,
            &upgrade,
        );
        match refused {
            Err(lash_kernel_state::paged::HelperUpgradeCheckError::Refused(
                lash_kernel_doc::HelperUpgradeRefusal::RetainedCallable { function },
            )) if !branded => assert_eq!(function, old_next),
            Err(lash_kernel_state::paged::HelperUpgradeCheckError::Refused(
                lash_kernel_doc::HelperUpgradeRefusal::BrandedData { function },
            )) if branded => assert_eq!(function, old_owner),
            other => panic!(
                "iterator upgrade must refuse its exact retained state, branded={branded}: {other:?}"
            ),
        }
        let mut resumed =
            KernelMachine::open(program(), BOUNDS, &checkpoint, Box::new(pages)).unwrap();
        resumed
            .deliver(effect.wait, lash_kernel_vm::Outcome::Completed(Datum::Null))
            .unwrap();
        let Step::Ended(lash_kernel_vm::End::Finished(result)) =
            resumed.run(&mut World, u64::MAX).unwrap()
        else {
            panic!("old iterator completes")
        };
        assert_eq!(result.result, Datum::Int(7.into()));
    }
    let active = fixture();
    let twice = active.ids["twice"];
    let deferred = active
        .checkpoint
        .check_helper_upgrade(
            Box::new(active.pages.clone()),
            &[(active.link.identity(), active.document.as_ref().clone())].into(),
            &Default::default(),
            &active.helpers,
            &lash_kernel_doc::HelperUpgrade {
                changed: [twice].into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        matches!(deferred, lash_kernel_doc::HelperUpgradeSafety::Deferred { functions } if functions.contains(&twice)),
        "a still-active helper must drain before upgrade"
    );
}
