//! Named package-model laws exercised without runners or resolver callbacks.
use crate::{LinkRefusal, LinkedContent, Namespace, OperationNeed, ReachSelection};
use lash_kernel_doc::{
    Datum, Document, Export, FunctionDefinition, FunctionExport, NumberPolicy, StartConvention,
    parse_definition,
};

use lash_vocabulary::types::Type;
use lash_vocabulary::{
    names::*,
    operations::{ContractId, OperationRequirement},
    package::*,
    providers::FormatVersion,
};
use std::collections::{BTreeMap, BTreeSet};
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
                    controls: lash_vocabulary::TurnControls::none(),
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
    let id = content.insert_document(doc.clone()).unwrap();
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
    let id = content.insert_document(doc.clone()).unwrap();
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
        )
        .unwrap();
    let second = content
        .inspect(
            roots[1],
            &ReachSelection::Start(StartSelector::Export(Name::new("run"))),
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
    let id = content.insert_document(doc.clone()).unwrap();
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
    let id = content.insert_document(doc).unwrap();
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
    let id = content.insert_document(doc.clone()).unwrap();
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
                &ReachSelection::Start(StartSelector::Export(Name::new("run")))
            )
            .unwrap()
            .signature
            .is_some()
    );
    assert_eq!(
        content
            .inspect(root, &ReachSelection::Start(StartSelector::Main))
            .unwrap_err(),
        LinkRefusal::MissingMain
    );
    let selector = StartSelector::Export(Name::new("run"));
    let args = BTreeMap::from([(Name::new("value"), Datum::Null)]);
    assert!(lash_kernel_check::validate_start_input(&doc, &selector, &args).is_err());
    assert!(lash_kernel_check::validate_start_result(&doc, &selector, &Datum::Null).is_err());
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
    let document = content.insert_document(doc.clone()).unwrap();
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
    let document = content.insert_document(doc.clone()).unwrap();
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
    let document = content.insert_document(doc.clone()).unwrap();
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
