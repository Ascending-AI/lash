//! The build image is an immutable library, with no package publication work.
use super::*;
#[path = "helper_classes.rs"]
mod helper_classes;

/// Release metadata opens without decoding helper bodies or recomputing identity.
#[test]
fn release_metadata_opens_without_helper_body_work() {
    let _ = lash_kernel_doc::document_work::helper_work();
    let release = helper_release().expect("build helper library");
    assert!(!release.roots.is_empty());
    for root in &release.roots {
        assert!(release.helpers.contains(root));
    }
    assert_eq!(
        lash_kernel_doc::document_work::helper_work(),
        lash_kernel_doc::document_work::HelperWork::default()
    );
    assert_eq!(
        release.helpers.identity(),
        lash_kernel_value::HelperLibraryId::of_members(release.helpers.members().copied())
    );
}

use crate::archive::{Archive, ArchiveError, Binding, HelperId, Library, NativeIdentity, Release};
use std::collections::BTreeSet;

fn archive_fixture(definition: lash_kernel_doc::FunctionDefinition) -> Release {
    let function = definition.identity();
    let natives = if definition.has_native() {
        BTreeMap::from([(
            function,
            NativeIdentity {
                package: "fixture".to_owned(),
                name: definition.name.to_string(),
                native_version: definition.native_version,
            },
        )])
    } else {
        BTreeMap::new()
    };
    let definitions = BTreeMap::from([(function, definition)]);
    let contract =
        archive::contract(function, &definitions, &BTreeMap::new()).expect("closed fixture");
    let helper = HelperId(format!("fixture:{}", definitions[&function].name));
    Release {
        natives,
        library: Library {
            identity: lash_kernel_value::HelperLibraryId::of_members([function]),
            members: BTreeSet::from([function]),
            roots: BTreeSet::from([function]),
        },
        definitions,
        libraries: BTreeMap::from([(
            "fixture".to_owned(),
            Library {
                identity: lash_kernel_value::HelperLibraryId::of_members([function]),
                members: BTreeSet::from([function]),
                roots: BTreeSet::from([function]),
            },
        )]),
        bindings: BTreeMap::from([(
            helper,
            Binding {
                revision: contract.revision(),
                function,
            },
        )]),
        ..Release::default()
    }
}

fn definition_fixture() -> lash_kernel_doc::FunctionDefinition {
    lash_kernel_doc::parse_definition_with_locals(
        "function helper(value: Int) -> Int\nkernel 1\ncharge 1\nbody { return value }\n",
    )
    .expect("a fixture definition")
    .0
}

/// FIG-7504: changing the full guard limit, while keeping its unit and shape,
/// cannot reuse the old contract revision. Appending a revision admits it.
#[test]
fn changed_contract_requires_an_appended_revision() {
    let mut definition = definition_fixture();
    definition.implementation = lash_kernel_doc::Implementation::Native {
        charge: lash_kernel_doc::Formula::Constant(1),
    };
    definition.guard = Some(lash_kernel_doc::Guard {
        unit: "step".to_owned(),
        limit: lash_kernel_doc::Formula::Constant(10),
    });
    let first = Archive::default()
        .append(archive_fixture(definition.clone()))
        .expect("first release");
    definition.guard.as_mut().expect("guard").limit = lash_kernel_doc::Formula::Constant(11);
    let next = first
        .append(archive_fixture(definition))
        .expect("new contract revision");
    first.check_append(&next).expect("append is permitted");
    let helper = HelperId("fixture:helper".to_owned());
    let mut violated = next;
    violated
        .ledger
        .get_mut(&helper)
        .expect("entry")
        .revisions
        .pop();
    assert_eq!(violated.validate(), Err(ArchiveError::Contract(helper)));
}

/// FIG-7504: a tombstone is permanent across nonadjacent releases. Reintroducing
/// the same contract is allowed; reusing the name for another contract refuses.
#[test]
fn tombstoned_helper_cannot_return_with_a_different_contract() {
    let definition = definition_fixture();
    let first = Archive::default()
        .append(archive_fixture(definition.clone()))
        .expect("first release");
    let removed = first.append(Release::default()).expect("tombstone");
    removed
        .append(archive_fixture(definition.clone()))
        .expect("same contract can return");
    let mut changed = definition;
    changed.errors.insert("new_error".to_owned());
    assert_eq!(
        removed.append(archive_fixture(changed)),
        Err(ArchiveError::Contract(HelperId(
            "fixture:helper".to_owned()
        )))
    );
}

/// FIG-7504: keeping a native's current version does not satisfy an archived
/// declaration of the old version, even when package and name are identical.
#[test]
fn deleting_a_retained_native_version_fails_release_check() {
    let mut definition = definition_fixture();
    definition.implementation = lash_kernel_doc::Implementation::Native {
        charge: lash_kernel_doc::Formula::Constant(1),
    };
    let first = Archive::default()
        .append(archive_fixture(definition.clone()))
        .expect("native v1");
    definition.native_version = 2;
    let next = first
        .append(archive_fixture(definition))
        .expect("native v2");
    let v1 = NativeIdentity {
        package: "fixture".to_owned(),
        name: "helper".to_owned(),
        native_version: 1,
    };
    let v2 = NativeIdentity {
        native_version: 2,
        ..v1.clone()
    };
    next.check_natives(&BTreeSet::from([v1.clone(), v2.clone()]))
        .expect("both implementations");
    assert_eq!(
        next.check_natives(&BTreeSet::from([v2])),
        Err(ArchiveError::Native(v1))
    );
}

/// FIG-7504: archive envelopes cannot authenticate corrupted definition bytes,
/// and an authentic member cannot call a missing exact callee.
#[test]
fn archive_authenticates_members_and_requires_closed_libraries() {
    let definition = definition_fixture();
    let first = Archive::default()
        .append(archive_fixture(definition.clone()))
        .expect("valid archive");
    let mut corrupt = first.clone();
    corrupt.releases[0]
        .definitions
        .get_mut(&definition.identity())
        .expect("definition")
        .errors
        .insert("corrupt".to_owned());
    assert_eq!(
        corrupt.validate(),
        Err(ArchiveError::Hash(definition.identity()))
    );
    let empty = HelperRelease {
        helpers: BuildHelpers::empty(),
        functions: FunctionRegistry::declarations(),
        roots: BTreeSet::new(),
        labels: BTreeMap::new(),
    };
    assert_eq!(
        with_archive(empty, &corrupt, &["fixture".to_owned()].into()).unwrap_err(),
        LibraryError::HelperUnavailable {
            function: definition.identity(),
            reason: lash_kernel_value::HelperUnavailableReason::Corrupt
        }
    );
    let mut dangling = definition;
    match &mut dangling.implementation {
        lash_kernel_doc::Implementation::Guest { body, .. } => {
            body.functions.insert(
                FunctionId::from_bytes([77; 32]),
                FunctionName::new("missing").expect("valid function name"),
            );
        }
        _ => unreachable!("guest fixture"),
    }
    let function = dangling.identity();
    let members = BTreeSet::from([function]);
    let violated = Archive {
        releases: vec![Release {
            definitions: BTreeMap::from([(function, dangling)]),
            libraries: BTreeMap::from([(
                "fixture".to_owned(),
                Library {
                    identity: lash_kernel_value::HelperLibraryId::of_members([function]),
                    members: members.clone(),
                    roots: members,
                },
            )]),
            ..Release::default()
        }],
        ..Archive::default()
    };
    assert!(matches!(violated.validate(), Err(ArchiveError::Closure(_))));
    let mut unlisted = definition_fixture();
    let lash_kernel_doc::Implementation::Guest { body, .. } = &mut unlisted.implementation else {
        unreachable!("guest fixture")
    };
    body.block = vec![lash_kernel_doc::Stmt::Return {
        value: lash_kernel_doc::Expr::Call {
            function: FunctionId::from_bytes([78; 32]),
            args: vec![],
        },
    }];
    let function = unlisted.identity();
    let unlisted_archive = Archive {
        releases: vec![Release {
            definitions: BTreeMap::from([(function, unlisted)]),
            ..Release::default()
        }],
        ..Archive::default()
    };
    assert!(matches!(
        unlisted_archive.validate(),
        Err(ArchiveError::Closure(_))
    ));
    let mut wrong_members = first;
    wrong_members.releases[0]
        .libraries
        .get_mut("fixture")
        .expect("library")
        .identity = lash_kernel_value::HelperLibraryId::of_members([]);
    assert_eq!(
        wrong_members.validate(),
        Err(ArchiveError::Closure("fixture".to_owned()))
    );
}

/// FIG-7504: removing or rewriting old artifacts or ledger revisions is rejected,
/// even if the edited tree is internally self-consistent.
#[test]
fn release_history_can_only_append() {
    let first = Archive::default()
        .append(archive_fixture(definition_fixture()))
        .expect("release");
    assert_eq!(
        first.check_append(&Archive::default()),
        Err(ArchiveError::Rewritten)
    );
    let mut rewritten = first.clone();
    rewritten.ledger.clear();
    assert_eq!(first.check_append(&rewritten), Err(ArchiveError::Rewritten));
}

/// The release check consumes the entire checked-in history, not only current
/// hashes. Native availability is checked by the engine's actual Rust constructors.
#[test]
#[cfg(not(feature = "synthetic-next"))]
fn checked_archive_covers_current_release_and_native_implementations() {
    let archive = helper_archive().expect("checked archive");
    let candidate = archive_candidate().expect("build candidate");
    assert!(
        archive == candidate,
        "record the release with record-helper-data record_archive"
    );
    let definitions = archive.validate().expect("authenticated definitions");
    let bindings: BTreeMap<_, _> = archive
        .releases
        .iter()
        .flat_map(|release| release.natives.clone())
        .collect();
    let mut registry = FunctionRegistry::declarations();
    let mut pending = definitions;
    let mut available = BTreeSet::new();
    while !pending.is_empty() {
        let function = *pending
            .iter()
            .find(|(_, definition)| {
                definition
                    .body()
                    .is_none_or(|body| body.functions.keys().all(|id| registry.get(id).is_some()))
            })
            .expect("authenticated Merkle graph is acyclic")
            .0;
        let definition = pending.remove(&function).expect("pending definition");
        let native = if let Some(identity) = bindings.get(&function) {
            let callees: Vec<_> = definition
                .body()
                .into_iter()
                .flat_map(|body| body.functions.iter())
                .map(|(id, name)| (*id, name.as_str()))
                .collect();
            let native = lash_kernel_engine::embedding::native_implementation(
                (
                    identity.package.as_str(),
                    identity.name.as_str(),
                    identity.native_version,
                ),
                function,
                &callees,
                &registry,
            );
            if native.is_some() {
                available.insert(identity.clone());
            }
            native
        } else {
            None
        };
        registry
            .register(definition, native)
            .expect("archived definition registers");
    }
    archive
        .check_natives(&available)
        .expect("all archived native versions remain implemented in Rust");
}

/// FIG-7512 law 1: an unchanged document resolves an archived helper after its
/// current body changes. Source lookup still chooses the current definition.
#[test]
fn an_orphan_document_keeps_its_identity_and_archived_body() {
    let old = definition_fixture();
    let old_id = old.identity();
    let archive = Archive::default()
        .append(archive_fixture(old.clone()))
        .unwrap();
    let current = lash_kernel_doc::parse_definition_with_locals(
        "function helper(value: Int) -> Int\nkernel 1\ncharge 1\nbody { return 99 }\n",
    )
    .unwrap()
    .0;
    let mut registry = FunctionRegistry::declarations();
    let current_id = registry.register(current, None).unwrap();
    let release = with_archive(
        HelperRelease {
            helpers: Arc::new(BuildHelpers::new(registry.clone(), [current_id]).unwrap()),
            functions: registry,
            roots: [current_id].into(),
            labels: Default::default(),
        },
        &archive,
        &["fixture".to_owned()].into(),
    )
    .unwrap();
    let mut document = lash_kernel_doc::parse_document(&format!(
        "kernel 1\nnumbers by_spelling\ndefinition {}\nmain {{ let answer = invoke helper(7) return answer }}",
        serde_json::to_string("function helper(value: Int) -> Int\nkernel 1\ncharge 1\nbody { return value }\n").unwrap()
    )).unwrap();
    document.functions.remove(&old_id);
    document.library = lash_kernel_doc::LibraryDependencies([old_id].into());
    document
        .requires
        .libraries
        .insert(archive.releases[0].library.identity);
    let identity = document.identity();
    lash_kernel_doc::validate_document(&document, release.helpers.as_ref()).unwrap();
    assert_eq!(document.identity(), identity);
    assert_eq!(
        run_document(document.clone(), &release),
        lash_kernel_doc::Datum::Int(7.into())
    );
    assert_eq!(
        release
            .helpers
            .registered(&old_id)
            .unwrap()
            .definition()
            .as_ref(),
        &old
    );
    assert_eq!(release.resolver_roots(), [current_id].into());
}

fn run_document(
    document: lash_kernel_doc::Document,
    release: &HelperRelease,
) -> lash_kernel_doc::Datum {
    run_selected(document, release, None, vec![])
}
fn run_selected(
    document: lash_kernel_doc::Document,
    release: &HelperRelease,
    callable: Option<lash_kernel_doc::FunctionValue>,
    args: Vec<lash_kernel_doc::Datum>,
) -> lash_kernel_doc::Datum {
    use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
    use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
    use lash_kernel_vm::{Machine as _, Step};
    #[derive(Default)]
    struct Pages(BTreeMap<PageId, PageAnswer>);
    impl PageSource for Pages {
        fn read(
            &mut self,
            pages: &[PageRef],
        ) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
            Ok(pages
                .iter()
                .map(|page| {
                    self.0
                        .get(&page.id)
                        .cloned()
                        .ok_or(HeapFault::Missing { page: page.id })
                })
                .collect())
        }
        fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
            let mut bytes = body.kind.0.to_be_bytes().to_vec();
            bytes.extend_from_slice(&body.tokens);
            let page = PageRef {
                id: PageId(lash_kernel_doc::heap_values::key_digest(&bytes)),
                grant: PageGrant::NONE,
            };
            self.0.insert(
                page.id,
                PageAnswer {
                    page,
                    kind: body.kind,
                    tokens: body.tokens.into(),
                    children: body.children,
                },
            );
            Ok(page)
        }
    }
    struct Host;
    impl lash_kernel_vm::Host for Host {
        fn print(&mut self, _: &lash_kernel_doc::Datum) {
            panic!("no prints")
        }
        fn cancel_requested(&mut self) -> bool {
            false
        }
    }
    let link = lash_kernel_value::Link {
        helper_fills: Default::default(),
        document: document.identity(),
        fills: BTreeMap::new(),
    };
    let program = lash_kernel_vm::Program::prepare(
        link.identity(),
        [(document.identity(), Arc::new(document))].into(),
        &[(link.identity(), link)].into(),
        PageId([0; 32]),
        lash_kernel_vm::PreparedLibrary::with_helpers(
            Arc::new(release.functions.clone()),
            Arc::clone(&release.helpers),
        ),
        lash_kernel_doc::Selection::Fresh,
    )
    .unwrap();
    let mut machine = lash_kernel_vm::KernelMachine::start(
        program,
        lash_kernel_vm::Bounds {
            memory_meter: lash_kernel_vm::MemoryMeter::On,
            charge: lash_kernel_vm::Charge::Exact,
            charge_limit: 1_000_000,
            memory: 64 << 20,
            call_depth: 100,
            live_tasks: 100,
            requests_per_park: 100,
            join_members: 100,
        },
        lash_kernel_vm::CellStart {
            owner: PageId([9; 32]),
            target: callable.map_or(lash_kernel_vm::Target::Main, |value| {
                lash_kernel_vm::Target::Callable(Box::new(value))
            }),
            args,
            session: None,
            modes: Default::default(),
            host: vec![],
        },
        Box::<Pages>::default(),
    )
    .unwrap();
    match machine.run(&mut Host, u64::MAX).unwrap() {
        Step::Ended(lash_kernel_vm::End::Finished(done)) => done.result,
        other => panic!("unexpected {other:?}"),
    }
}

/// A cached reach records the maximum first generation, not a token for each helper.
#[test]
fn helper_claims_use_the_first_generation_and_join_at_the_maximum() {
    let old = definition_fixture();
    let newer = lash_kernel_doc::parse_definition(
        "function helper(value: Int) -> Int\nkernel 1\ncharge 1\nbody { return 99 }\n",
    )
    .unwrap();
    let old_id = old.identity();
    let new_id = newer.identity();
    let archive = Archive::default()
        .append(archive_fixture(old))
        .unwrap()
        .append(archive_fixture(newer))
        .unwrap();
    let release = with_archive(
        HelperRelease {
            helpers: BuildHelpers::empty(),
            functions: FunctionRegistry::declarations(),
            roots: BTreeSet::new(),
            labels: BTreeMap::new(),
        },
        &archive,
        &["fixture".to_owned()].into(),
    )
    .unwrap();
    let mut required = lash_kernel_value::claims::NodeRequirements::default();
    release.helpers.require(new_id, &mut required);
    release.helpers.require(old_id, &mut required);
    let lineage = archive.releases[0].library.identity;
    assert_eq!(release.helpers.claim_generations()[&old_id].generation, 1);
    assert_eq!(release.helpers.claim_generations()[&new_id].generation, 2);
    assert_eq!(*required.helpers.archives(), [(lineage, 2)].into());
    assert!(required.helpers.outside().is_empty());
    assert_eq!(
        required.libraries,
        [archive.releases[0].libraries["fixture"].identity].into()
    );
    assert!(
        required
            .helpers
            .unmet_by(
                &release.helpers.claim_capabilities(),
                release.helpers.library_identities()
            )
            .is_empty()
    );
    let extra = lash_kernel_doc::parse_definition(
        "function additional(value: Int) -> Int\nkernel 1\ncharge 1\nbody { return value }\n",
    )
    .unwrap();
    let extra_id = extra.identity();
    let additional = Archive::default().append(archive_fixture(extra)).unwrap();
    let combined = with_archive(release, &additional, &["fixture".to_owned()].into()).unwrap();
    assert_eq!(combined.helpers.claim_generations()[&old_id].generation, 1);
    assert_eq!(combined.helpers.claim_generations()[&new_id].generation, 2);
    assert!(combined.helpers.claim_generations().contains_key(&extra_id));
}

/// FIG-7523 law 2: an explicit equal-contract revision runs 99; the original still runs 7.
#[test]
fn explicit_helper_upgrade_runs_new_body_and_refuses_a_changed_contract() {
    let old = definition_fixture();
    let old_id = old.identity();
    let current = lash_kernel_doc::parse_definition(
        "function helper(value: Int) -> Int\nkernel 1\ncharge 2\nbody { return 99 }",
    )
    .unwrap();
    let current_id = current.identity();
    let archive = Archive::default()
        .append(archive_fixture(old.clone()))
        .unwrap()
        .append(archive_fixture(current.clone()))
        .unwrap();
    let relay = format!(
        "function relay(value: Int) -> Int\nkernel 1\nmetering statements\nuse helper = @{old_id}\nbody {{ let answer = invoke helper(value) return answer }}"
    );
    let mut document = lash_kernel_doc::parse_document(&format!(
        "kernel 1\nnumbers by_spelling\ndefinition {}\ndefinition {}\nmain {{ let answer = invoke relay(7) return answer }}",
        serde_json::to_string("function helper(value: Int) -> Int\nkernel 1\ncharge 1\nbody { return value }").unwrap(), serde_json::to_string(&relay).unwrap()
    )).unwrap();
    document.functions.remove(&old_id);
    document.library = lash_kernel_doc::LibraryDependencies([old_id].into());
    let original = document.identity();
    let pairing = archive.upgrade(&[old_id].into()).unwrap();
    let already_current = archive.upgrade(&[current_id].into()).unwrap();
    assert!(already_current.changes.is_empty());
    assert!(
        already_current.changed.is_empty(),
        "a no-op changes no constructor or retained callable"
    );
    let rewritten = lash_kernel_migrate::upgrade_helpers(&document, &pairing).unwrap();
    assert_ne!(rewritten.document.identity(), original);
    assert_eq!(
        pairing.changes,
        vec![lash_kernel_doc::HelperChange {
            helper: HelperId("fixture:helper".into()),
            from: old_id,
            to: current_id,
            body: true,
            charge: true,
            native_version: false
        }]
    );
    let mut registry = FunctionRegistry::declarations();
    registry.register(old, None).unwrap();
    registry.register(current.clone(), None).unwrap();
    let release = HelperRelease {
        helpers: Arc::new(BuildHelpers::new(registry.clone(), [old_id, current_id]).unwrap()),
        functions: registry,
        roots: [current_id].into(),
        labels: Default::default(),
    };
    assert_eq!(
        run_document(document.clone(), &release),
        lash_kernel_doc::Datum::Int(7.into())
    );
    assert_eq!(
        run_document(rewritten.document, &release),
        lash_kernel_doc::Datum::Int(99.into())
    );
    assert_eq!(document.identity(), original);
    let future = lash_kernel_doc::parse_definition(
        "function helper(value: Int) -> Int\nkernel 1\ncharge 3\nbody { return 101 }",
    )
    .unwrap();
    let future_id = future.identity();
    let later = archive
        .clone()
        .append(archive_fixture(future.clone()))
        .unwrap();
    let pinned = later
        .upgrade_to(&[old_id].into(), archive.releases[1].library.identity)
        .unwrap();
    let mut future_release = release.clone();
    future_release.functions.register(future, None).unwrap();
    future_release.helpers = Arc::new(
        BuildHelpers::new(
            future_release.functions.clone(),
            [old_id, current_id, future_id],
        )
        .unwrap(),
    );
    let recorded = lash_kernel_migrate::upgrade_helpers(&document, &pinned).unwrap();
    assert_eq!(
        run_document(recorded.document, &future_release),
        lash_kernel_doc::Datum::Int(99.into()),
        "a recorded target must not be substituted by the later 101 body"
    );
    // A published closure owns frozen scalar data; creating a revision preserves that data.
    let factory_text = format!(
        "function factory(marker: Any) -> Any\nkernel 1\nmetering statements\nuse helper = @{old_id}\nbody {{ let held = [7, marker] return fn(x) presentation (x: Int) -> Tuple(Int, Int) bare direct {{ let seed = held[0] let answer = invoke helper(x) return (seed, answer) }} }}"
    );
    let (factory, locals) = lash_kernel_doc::parse_definition_with_locals(&factory_text).unwrap();
    let factory_id = factory.identity();
    let factory_signature = factory.signature.clone();
    let selected = locals[0].identity();
    let mut captured_document =
        lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::BySpelling, vec![]);
    captured_document.main = None;
    captured_document.exports.insert(
        lash_kernel_value::Name::new("factory"),
        lash_kernel_doc::Export::Function(lash_kernel_doc::FunctionExport {
            function: factory_id,
            signature: factory_signature,
            convention: lash_kernel_doc::StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    captured_document.functions = locals
        .into_iter()
        .chain([factory])
        .map(|definition| (definition.identity(), definition))
        .collect();
    captured_document.library.insert(old_id);
    let old_link = lash_kernel_value::Link {
        document: captured_document.identity(),
        fills: Default::default(),
        helper_fills: Default::default(),
    }
    .identity();
    let frozen = lash_kernel_doc::FunctionValue {
        written_signature: None,
        signature_origin: None,
        implicit_text_conversions: vec![],
        callable: lash_kernel_value::CallableId {
            link: old_link,
            function: selected,
        },
        captures: [(
            lash_kernel_value::Name::new("held"),
            lash_kernel_doc::CaptureValue::List(vec![
                lash_kernel_doc::CaptureValue::Data(lash_kernel_doc::Datum::Int(7.into())),
                lash_kernel_doc::CaptureValue::Data(lash_kernel_doc::Datum::Null),
            ]),
        )]
        .into(),
        signature: lash_kernel_doc::parse_signature("(x: Int) -> Tuple(Int, Int)").unwrap(),
        convention: lash_kernel_doc::StartConvention::Direct,
        captions: lash_kernel_doc::Captions::empty(captured_document.identity()),
        representation: lash_kernel_doc::FunctionRepresentation::Bare,
        creation: Some(lash_kernel_value::sites::Site::new(
            lash_kernel_value::sites::Unit::Library(factory_id),
            [1, 0],
        )),
        token: None,
    };
    let upgraded_document =
        lash_kernel_migrate::upgrade_helpers(&captured_document, &pairing).unwrap();
    let new_link = lash_kernel_value::Link {
        document: upgraded_document.document.identity(),
        fills: Default::default(),
        helper_fills: Default::default(),
    }
    .identity();
    let upgraded_value = lash_kernel_migrate::upgrade_callable(
        &frozen,
        &captured_document,
        &upgraded_document,
        new_link,
        &pairing,
    )
    .unwrap();
    assert_eq!(
        run_selected(
            captured_document.clone(),
            &release,
            Some(frozen.clone()),
            vec![lash_kernel_doc::Datum::Int(11.into())]
        ),
        lash_kernel_doc::Datum::Tuple(vec![
            lash_kernel_doc::Datum::Int(7.into()),
            lash_kernel_doc::Datum::Int(11.into())
        ])
    );
    assert_eq!(
        run_selected(
            upgraded_document.document.clone(),
            &release,
            Some(upgraded_value),
            vec![lash_kernel_doc::Datum::Int(11.into())]
        ),
        lash_kernel_doc::Datum::Tuple(vec![
            lash_kernel_doc::Datum::Int(7.into()),
            lash_kernel_doc::Datum::Int(99.into())
        ])
    );
    let mut branded = frozen;
    let lash_kernel_doc::CaptureValue::List(held) = branded
        .captures
        .get_mut(&lash_kernel_value::Name::new("held"))
        .unwrap()
    else {
        unreachable!()
    };
    held[1] = lash_kernel_doc::CaptureValue::Data(lash_kernel_doc::Datum::Record(vec![
        (
            "__lash_brand".into(),
            lash_kernel_doc::Datum::Tuple(vec![
                lash_kernel_doc::Datum::Text("fixture".into()),
                lash_kernel_doc::Datum::Text("value".into()),
            ]),
        ),
        (
            "__lash_built".into(),
            lash_kernel_doc::Datum::Bytes(lash_kernel_doc::Bytes::new(old_id.as_bytes().to_vec())),
        ),
    ]));
    if let lash_kernel_doc::CaptureValue::Data(argument) = &held[1] {
        assert!(matches!(
            lash_kernel_migrate::check_upgrade_data(argument, &pairing),
            Err(lash_kernel_doc::HelperUpgradeRefusal::BrandedData { function }) if function == old_id
        ));
    } else {
        panic!("fixture argument is data");
    }
    assert!(
        matches!(lash_kernel_migrate::upgrade_callable(&branded, &captured_document, &upgraded_document, new_link, &pairing), Err(lash_kernel_doc::HelperUpgradeRefusal::BrandedData {function}) if function == old_id)
    );
    let mut changed = current;
    changed.errors.insert("new_error".into());
    let removed = archive.clone().append(Release::default()).unwrap();
    assert!(
        matches!(removed.upgrade(&[old_id].into()), Err(lash_kernel_doc::HelperUpgradeRefusal::HelperTombstoned { helper }) if helper == HelperId("fixture:helper".into()))
    );
    let old_revision = archive.releases[0].bindings[&HelperId("fixture:helper".into())].revision;
    let changed_archive = archive.append(archive_fixture(changed)).unwrap();
    let target_revision = changed_archive.releases.last().unwrap().bindings
        [&HelperId("fixture:helper".into())]
        .revision;
    assert!(
        matches!(changed_archive.upgrade(&[old_id].into()), Err(lash_kernel_doc::HelperUpgradeRefusal::HelperContractChanged { helper, from, to }) if helper == HelperId("fixture:helper".into()) && from == old_revision && to == target_revision)
    );
}
