//! Named package-model laws exercised without runners or resolver callbacks.
use crate::{LinkRefusal, LinkedContent, Namespace, OperationNeed, ReachSelection};
use lash_kernel_doc::{
    Datum, Document, Export, FunctionDefinition, FunctionExport, NumberPolicy, StartConvention,
    parse_definition,
};

use lash_kernel_value::types::Type;
use std::collections::{BTreeMap, BTreeSet};
use {
    lash_kernel_value::names::*, lash_kernel_value::operations::ContractId,
    lash_kernel_value::operations::OperationRequirement, lash_kernel_value::package::*,
    lash_kernel_value::providers::FormatVersion,
};
fn path(text: &str) -> PackagePath {
    PackagePath::parse(text).unwrap()
}
fn segment(text: &str) -> Segment {
    Segment::new(text).unwrap()
}
fn context(root: &str, publisher: &str) -> PublicationContext {
    PublicationContext {
        domain: PublicationDomain("test".into()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance(publisher.into()),
        root: segment(root),
    }
}
fn source() -> Document {
    let function=parse_definition("function app.run(value: Text) -> Text\nkernel 1\ncharge 1\nimport 0 operation service.read(value: Text) -> Text\nbody { let answer = perform 0(value) as Text return answer }").unwrap();
    document(function)
}
fn document(function: FunctionDefinition) -> Document {
    let mut document = Document::new(NumberPolicy::Float, vec![]);
    document.main = None;
    document.imports = function.body().unwrap().imports.clone();
    document.exports.insert(
        Name::new("run"),
        Export::Function(FunctionExport {
            function: function.identity(),
            signature: function.signature.clone(),
            convention: StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    document.functions.insert(function.identity(), function);
    document
}
fn service(document: &Document, contract: &str, visibility: Visibility) -> Package {
    let ImportKind::Operation { signature } = &document.imports[&ImportSlot(0)].kind else {
        unreachable!()
    };
    Package {
        path: path("service"),
        members: BTreeMap::from([(
            segment("read"),
            Member {
                visibility,
                kind: MemberKind::Operation(OperationOffer {
                    requirement: OperationRequirement {
                        signature: signature.clone(),
                        contract: ContractId::new(contract),
                    },
                    intents: BTreeSet::new(),
                    controls: lash_kernel_value::TurnControls::none(),
                    mode: OperationMode::Waited,
                }),
            },
        )]),
    }
}
#[test]
fn l_admit_contract_fresh_fulfilment_accepts_c2_but_existing_link_keeps_c1() {
    let doc = source();
    let mut content = LinkedContent::default();
    let id = content
        .insert_document(doc.clone(), &doc.functions)
        .unwrap();
    let owner = context("service", "provider");
    let consumer = context("app", "host");
    let a = PackageAdvertisement::Enumerated(service(&doc, "c1", Visibility::Public));
    let b = PackageAdvertisement::Enumerated(service(&doc, "c2", Visibility::Public));
    let old = Namespace {
        advertisements: vec![(&owner, &a)],
    }
    .fulfil(&content, id, &consumer, None, None)
    .unwrap();
    let current = Namespace {
        advertisements: vec![(&owner, &b)],
    };
    let fresh = current.fulfil(&content, id, &consumer, None, None).unwrap();
    assert_ne!(old.identity(), fresh.identity());
    assert_eq!(old.document, fresh.document);
    assert!(matches!(
        current.validate_link(&content, &old, &consumer, None, None),
        Err(LinkRefusal::ContractMismatch { .. })
    ));
    assert_eq!(doc.identity(), id);
}
#[test]
fn l_dispatch_one_document_linked_twice_keeps_independent_operation_uses() {
    let doc = source();
    let mut content = LinkedContent::default();
    let id = content
        .insert_document(doc.clone(), &doc.functions)
        .unwrap();
    let owner = context("service", "provider");
    let consumer = context("app", "host");
    let mut roots = vec![];
    for contract in ["c1", "c2"] {
        let ad = PackageAdvertisement::Enumerated(service(&doc, contract, Visibility::Public));
        let link = Namespace {
            advertisements: vec![(&owner, &ad)],
        }
        .fulfil(&content, id, &consumer, None, None)
        .unwrap();
        roots.push(content.insert_link(link).unwrap());
    }
    let first = content
        .inspect(
            roots[0],
            &ReachSelection::Start(StartSelector::Export(Name::new("run"))),
            &BTreeMap::new(),
        )
        .unwrap();
    let second = content
        .inspect(
            roots[1],
            &ReachSelection::Start(StartSelector::Export(Name::new("run"))),
            &BTreeMap::new(),
        )
        .unwrap();
    let pins = first
        .operations
        .into_iter()
        .chain(second.operations)
        .collect::<BTreeMap<_, _>>();
    assert_eq!(pins.len(), 2);
    for (use_, need) in pins {
        let OperationNeed::Fixed { requirement, .. } = need else {
            panic!("fixed operation")
        };
        assert_eq!(
            requirement.contract,
            ContractId::new(if use_.link == roots[0] { "c1" } else { "c2" })
        );
    }
}
#[test]
fn l_internal_requires_exact_package_and_publisher() {
    let doc = source();
    let mut content = LinkedContent::default();
    let id = content
        .insert_document(doc.clone(), &doc.functions)
        .unwrap();
    let owner = context("service", "owner");
    let foreign = context("service", "foreign");
    let ad = PackageAdvertisement::Enumerated(service(&doc, "c1", Visibility::Internal));
    let ns = Namespace {
        advertisements: vec![(&owner, &ad)],
    };
    assert!(
        ns.fulfil(&content, id, &owner, Some(&path("service")), None)
            .is_ok()
    );
    assert!(matches!(
        ns.fulfil(&content, id, &foreign, Some(&path("service")), None),
        Err(LinkRefusal::NotVisible { .. })
    ));
    assert!(matches!(
        ns.fulfil(&content, id, &owner, Some(&path("service.child")), None),
        Err(LinkRefusal::NotVisible { .. })
    ));
    assert!(matches!(
        ns.fulfil(&content, id, &owner, None, None),
        Err(LinkRefusal::NotVisible { .. })
    ));
}
#[test]
fn l_deferred_attach_pins_the_grant_without_resolving() {
    let function=parse_definition("function app.run(value: Json) -> Json\nkernel 1\ncharge 1\nimport 0 operation wip.fetch(value: Json) -> Json\nbody { let answer = perform 0(value) as Json return answer }").unwrap();
    let doc = document(function);
    let mut content = LinkedContent::default();
    let id = content
        .insert_document(doc.clone(), &doc.functions)
        .unwrap();
    let owner = context("wip", "resolver");
    let consumer = context("app", "host");
    let ad = PackageAdvertisement::Deferred(Box::new(DeferredPackage {
        path: path("wip"),
        visibility: Visibility::Public,
        provider: ProviderName {
            owner: "plugin".into(),
            key: "resolve".into(),
        },
        config: ProviderConfig {
            component: None,
            format: FormatVersion::ONE,
            value: serde_json::json!({"account":"original"}),
            payload: None,
        },
        resolution_policy: Default::default(),
    }));
    let ns = Namespace {
        advertisements: vec![(&owner, &ad)],
    };
    let mut attachment = Attachment {
        members: BTreeSet::new(),
        packages: BTreeSet::from([path("wip")]),
        deferred: BTreeSet::new(),
    };
    assert!(matches!(
        ns.fulfil(&content, id, &consumer, None, Some(&attachment)),
        Err(LinkRefusal::UndeclaredDeferred { .. })
    ));
    attachment.deferred.insert(path("wip"));
    let link = ns
        .fulfil(&content, id, &consumer, None, Some(&attachment))
        .unwrap();
    let Fill::Deferred { provider } = &link.fills[&ImportSlot(0)] else {
        panic!("deferred")
    };
    assert_eq!(provider.context, owner);
    assert_eq!(
        provider.config.value,
        serde_json::json!({"account":"original"})
    );
}
#[test]
fn l_start_sync_data_export_needs_no_main() {
    let doc = source();
    let mut content = LinkedContent::default();
    let id = content
        .insert_document(doc.clone(), &doc.functions)
        .unwrap();
    let ad = PackageAdvertisement::Enumerated(service(&doc, "c1", Visibility::Public));
    let owner = context("service", "provider");
    let consumer = context("app", "host");
    let link = Namespace {
        advertisements: vec![(&owner, &ad)],
    }
    .fulfil(&content, id, &consumer, None, None)
    .unwrap();
    let root = content.insert_link(link).unwrap();
    assert!(
        content
            .inspect(
                root,
                &ReachSelection::Start(StartSelector::Export(Name::new("run"))),
                &BTreeMap::new()
            )
            .unwrap()
            .signature
            .is_some()
    );
    assert_eq!(
        content
            .inspect(
                root,
                &ReachSelection::Start(StartSelector::Main),
                &BTreeMap::new()
            )
            .unwrap_err(),
        LinkRefusal::MissingMain
    );
    let selector = StartSelector::Export(Name::new("run"));
    let args = BTreeMap::from([(Name::new("value"), Datum::Null)]);
    assert!(lash_kernel_check::validate_start_input(&doc, &selector, root, &args).is_err());
    assert!(lash_kernel_check::validate_start_result(&doc, &selector, root, &Datum::Null).is_err());
}
#[test]
fn a4_matching_brand_with_stale_shape_is_catchable() {
    let brand = Brand {
        package: path("data"),
        name: segment("row"),
    };
    let error = crate::check_branded(
        &brand,
        &brand,
        &Type::Text,
        &Datum::Null,
        NumberPolicy::Float,
    )
    .unwrap_err();
    assert_eq!(error.kind, crate::STALE_VALUE);
    let other = Brand {
        package: path("other"),
        name: segment("row"),
    };
    assert_eq!(
        crate::check_branded(
            &brand,
            &other,
            &Type::Text,
            &Datum::Text("ok".into()),
            NumberPolicy::Float
        )
        .unwrap_err()
        .kind,
        "type_error"
    );
}
/// L-INTERNAL: a content hash cannot launder an internal fill into foreign publication authority.
#[test]
fn l_internal_forged_link_cannot_publish_another_owners_internal_fill() {
    let doc = source();
    let mut content = LinkedContent::default();
    let document = content
        .insert_document(doc.clone(), &doc.functions)
        .unwrap();
    let owner = context("service", "provider");
    let outsider = context("app", "foreign");
    let service = PackageAdvertisement::Enumerated(service(&doc, "c1", Visibility::Internal));
    let snapshot = Namespace {
        advertisements: vec![(&owner, &service)],
    };
    let forged = snapshot
        .fulfil(&content, document, &owner, Some(&path("service")), None)
        .unwrap();
    let link = content.insert_link(forged).unwrap();
    let counterfeit = Package {
        path: path("app"),
        members: BTreeMap::from([(
            segment("run"),
            Member {
                visibility: Visibility::Public,
                kind: MemberKind::Export {
                    link,
                    export: ExportName {
                        name: Name::new("run"),
                        member: None,
                    },
                },
            },
        )]),
    };
    assert!(matches!(
        content.validate_publication(&outsider, &counterfeit, &snapshot),
        Err(LinkRefusal::NotVisible { .. })
    ));
}

/// Sidecar keys belong to the exact Package and retained private definitions.
#[test]
fn label_keys_cannot_name_unknown_members_or_replace_public_member_labels() {
    let doc = source();
    let mut content = LinkedContent::default();
    let document = content
        .insert_document(doc.clone(), &doc.functions)
        .unwrap();
    let link = Link {
        document,
        fills: BTreeMap::from([(
            ImportSlot(0),
            Fill::Operation(OperationRequirement {
                signature: match &doc.imports[&ImportSlot(0)].kind {
                    ImportKind::Operation { signature } => signature.clone(),
                    _ => unreachable!(),
                },
                contract: ContractId::new("c1"),
            }),
        )]),
    };
    let link = content.insert_link(link).unwrap();
    let package = Package {
        path: path("app"),
        members: BTreeMap::from([(
            segment("run"),
            Member {
                visibility: Visibility::Public,
                kind: MemberKind::Export {
                    link,
                    export: ExportName {
                        name: Name::new("run"),
                        member: None,
                    },
                },
            },
        )]),
    };
    assert!(crate::read_labels(&content, &package, r#"{"members":{"run":"Run"}}"#).is_ok());
    let unknown =
        crate::read_labels(&content, &package, r#"{"members":{"missing":"Unknown"}}"#).unwrap_err();
    assert_eq!(unknown.pointer, "/members/missing");
    let public =
        crate::read_labels(&content, &package, r#"{"definitions":{"app.run":"Run"}}"#).unwrap_err();
    assert_eq!(public.pointer, "/definitions/app.run");
}

/// L-INTERNAL/L-PIN: authored origins cannot borrow a foreign internal fill or a new contract.
#[test]
fn authored_origin_requires_public_imports_and_the_accepted_contract_pin() {
    let doc = source();
    let mut content = LinkedContent::default();
    let document = content
        .insert_document(doc.clone(), &doc.functions)
        .unwrap();
    let provider = context("service", "provider");
    let public = PackageAdvertisement::Enumerated(service(&doc, "c1", Visibility::Public));
    let internal = PackageAdvertisement::Enumerated(service(&doc, "c1", Visibility::Internal));
    let changed = PackageAdvertisement::Enumerated(service(&doc, "c2", Visibility::Public));
    let namespace = Namespace {
        advertisements: vec![(&provider, &public)],
    };
    let link = namespace
        .fulfil(&content, document, &context("app", "host"), None, None)
        .unwrap();
    namespace.validate_authored_origin(&content, &link).unwrap();
    for denied in [&internal, &changed] {
        assert!(matches!(
            Namespace {
                advertisements: vec![(&provider, denied)]
            }
            .validate_authored_origin(&content, &link),
            Err(LinkRefusal::NotVisible { .. })
        ));
    }
    assert!(
        Namespace {
            advertisements: vec![]
        }
        .validate_authored_origin(&content, &link)
        .is_err()
    );
}

/// FIG-7135: aliases of one retained Document do not multiply the work of
/// indexing its private definition labels, even across distinct Links.
#[test]
fn label_definition_index_visits_each_document_once() {
    let mut doc = source();
    let hidden = parse_definition(
        "function app.hidden(value: Text) -> Text\nkernel 1\ncharge 1\nbody { return value }",
    )
    .unwrap();
    doc.functions.insert(hidden.identity(), hidden);
    let expected = doc.functions.len();
    assert!(expected > 0);
    let mut content = LinkedContent::default();
    let document = content
        .insert_document(doc.clone(), &doc.functions)
        .unwrap();
    let mut members = BTreeMap::new();
    for contract in ["c1", "c2"] {
        let link = Link {
            document,
            fills: [(
                ImportSlot(0),
                Fill::Operation(OperationRequirement {
                    signature: match &doc.imports[&ImportSlot(0)].kind {
                        ImportKind::Operation { signature } => signature.clone(),
                        _ => unreachable!(),
                    },
                    contract: ContractId::new(contract),
                }),
            )]
            .into(),
        };
        let link = content.insert_link(link).unwrap();
        members.insert(
            segment(contract),
            Member {
                visibility: Visibility::Public,
                kind: MemberKind::Export {
                    link,
                    export: ExportName {
                        name: Name::new("run"),
                        member: None,
                    },
                },
            },
        );
    }
    let package = Package {
        path: path("app"),
        members,
    };
    crate::labels::DEFINITION_VISITS.with(|count| count.set(0));
    crate::read_labels(
        &content,
        &package,
        r#"{"definitions":{"app.hidden":"Hidden"}}"#,
    )
    .unwrap();
    let visits = crate::labels::DEFINITION_VISITS.with(std::cell::Cell::get);
    assert!(
        visits <= expected,
        "label work must follow distinct Documents, not advertised aliases: visited {visits}, bound {expected}"
    );
}

/// Fresh inspection is contextual admission even when content was decoded
/// structurally. Missing declared helpers cannot disappear from required reach.
#[test]
fn fresh_inspection_refuses_missing_declared_helpers() {
    let mut document = Document::new(NumberPolicy::BySpelling, vec![]);
    document.library.insert(FunctionId::from_bytes([61; 32]));
    lash_kernel_doc::validate_document_structure(&document).unwrap();
    let link = Link {
        document: document.identity(),
        fills: BTreeMap::new(),
    };
    let root = link.identity();
    let content = LinkedContent {
        documents: [(document.identity(), document.into())].into(),
        links: [(root, link)].into(),
        ..Default::default()
    };
    assert!(matches!(
        content.inspect(
            root,
            &ReachSelection::AllExports,
            lash_kernel_doc::BuildHelpers::empty().as_ref()
        ),
        Err(LinkRefusal::InvalidDocument { .. })
    ));
    content
        .inspect_ordinary(root, &ReachSelection::AllExports)
        .expect("ordinary retained integrity needs no helper catalog");
}

/// MATERIALIZE-VALUES: opening a callable exposes frozen values as ordinary initializers.
#[test]
fn materialize_values_exposes_frozen_initializers() {
    use lash_kernel_doc::{
        Captions, CaptureValue, Expr, FunctionRepresentation, FunctionValue, Literal, Rhs, Stmt,
    };
    let definition =
        parse_definition("function app.saved() -> Int\nkernel 1\ncharge 1\nbody { return offset }")
            .unwrap();
    let doc = document(definition.clone());
    let link = Link {
        document: doc.identity(),
        fills: BTreeMap::new(),
    };
    let function = FunctionValue {
        callable: CallableId {
            link: link.identity(),
            function: definition.identity(),
        },
        captures: [(
            Name::new("offset"),
            CaptureValue::Data(Datum::Int(7.into())),
        )]
        .into(),
        signature: definition.signature.clone(),
        convention: StartConvention::Direct,
        representation: FunctionRepresentation::Bare,
        token: None,
        creation: None,
        written_signature: None,
        signature_origin: None,
        implicit_text_conversions: vec![],
        captions: Captions::empty(doc.identity()),
    };
    let reference = WorkflowReference::Callable(function.callable);
    let content = LinkedContent {
        documents: [(doc.identity(), doc.into())].into(),
        links: [(link.identity(), link)].into(),
        ..Default::default()
    };
    let opened = crate::materialize_workflow(
        &content,
        &reference,
        Some(&function),
        None,
        &lash_kernel_doc::BuildHelpers::empty(),
    )
    .unwrap();
    assert!(opened.document.functions.values().any(|definition| definition.body().is_some_and(|body| body.block.iter().any(|stmt| matches!(stmt, Stmt::Let { value: Rhs::Expr(Expr::Literal(Literal::Int(value))), .. } if value == &7.into())))), "MATERIALIZE-VALUES: capture 7 must live in the ordinary document");
}

/// L-CLAIM-LIBRARY: helper native debt is one exact image identity,
/// while an ordinary native retains its own capability token.
#[test]
fn helper_native_debt_is_owned_by_its_exact_library_manifest() {
    let definition =
        parse_definition("function helper.required() -> Null\nkernel 1\ncharge 1\nnative").unwrap();
    let function = definition.identity();
    let mut registry = lash_kernel_doc::FunctionRegistry::declarations();
    registry.register(definition, None).unwrap();
    let helpers = lash_kernel_doc::BuildHelpers::new(registry, [function]).unwrap();
    let mut doc = lash_kernel_doc::parse_document(&format!(
        "kernel 1\nnumbers float\nlibrary @{function}\nmain {{ let answer = invoke @{function}() return answer }}"
    )).unwrap();
    doc.requires.natives.insert(function);
    let link = Link {
        document: doc.identity(),
        fills: BTreeMap::new(),
    };
    let root = link.identity();
    let content = LinkedContent {
        documents: [(link.document, doc.into())].into(),
        links: [(root, link)].into(),
        ..Default::default()
    };
    let reach = content
        .inspect(root, &ReachSelection::Start(StartSelector::Main), &helpers)
        .unwrap();
    assert_eq!(reach.machine.libraries, [helpers.identity()].into());
    assert!(reach.machine.natives.is_empty());
}

/// FIG-7354: racing inspections through one owned memo share only exact
/// inputs and selection; a warm result cannot authorize changed code or an
/// absent current Link.
#[test]
fn concurrent_inspections_keep_selectors_and_current_inputs_isolated() {
    use std::sync::{Arc, Barrier};
    let first =
        parse_definition("function app.first() -> Null\nkernel 1\ncharge 1\nbody { return null }")
            .unwrap();
    let second =
        parse_definition("function app.second() -> Null\nkernel 1\ncharge 1\nbody { return null }")
            .unwrap();
    let ids = [first.identity(), second.identity()];
    let mut doc = document(first);
    doc.functions.insert(ids[1], second);
    let link = Link {
        document: doc.identity(),
        fills: BTreeMap::new(),
    };
    let root = link.identity();
    let mut content = LinkedContent {
        documents: [(link.document, Arc::new(doc))].into(),
        links: [(root, link.clone())].into(),
        ..Default::default()
    };
    let helpers = lash_kernel_doc::BuildHelpers::empty();
    let inspections = crate::Inspections::default();
    let barrier = Barrier::new(4);
    std::thread::scope(|scope| {
        let readers: Vec<_> = (0..4)
            .map(|index| {
                let content = &content;
                let inspections = &inspections;
                let helpers = &*helpers;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    let function = ids[index % 2];
                    let reach = inspections
                        .inspect(content, root, &ReachSelection::Callable(function), helpers)
                        .unwrap();
                    assert_eq!(reach.functions[&root], [function].into());
                })
            })
            .collect();
        for reader in readers {
            reader.join().unwrap();
        }
    });
    let mut changed = (*content.documents[&link.document]).clone();
    changed.library.insert(FunctionId::from_bytes([218; 32]));
    content.documents.insert(link.document, Arc::new(changed));
    assert!(matches!(
        inspections.inspect(&content, root, &ReachSelection::Callable(ids[0]), &helpers),
        Err(LinkRefusal::InvalidDocument { .. })
    ));
    content.links.remove(&root);
    assert!(matches!(
        inspections.inspect(&content, root, &ReachSelection::Callable(ids[0]), &helpers),
        Err(LinkRefusal::MissingLink { .. })
    ));
}
