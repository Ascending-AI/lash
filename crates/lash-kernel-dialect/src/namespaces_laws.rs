//! L-SUPERSET and L-DEFERRED-ATTACH at the source-name boundary.
use crate::{NamespaceKind, NamespaceMember, Namespaces, Resolution, Spelling};
use std::collections::{BTreeMap, BTreeSet};
use {
    lash_kernel_value::PackagePath, lash_kernel_value::Segment, lash_kernel_value::TurnControls,
    lash_kernel_value::package::OperationMode, lash_kernel_value::package::OperationOffer,
    lash_kernel_value::package::Visibility,
};
fn path(name: &str) -> PackagePath {
    PackagePath::parse(name).unwrap()
}
fn package() -> BTreeMap<Segment, NamespaceMember> {
    let name = crate::service_name("typescript", "clock");
    BTreeMap::from([(
        name.member.clone(),
        NamespaceMember {
            visibility: Visibility::Public,
            kind: NamespaceKind::Operation(
                OperationOffer {
                    requirement: lash_kernel_value::OperationRequirement::new(
                        lash_kernel_value::Signature {
                            params: vec![],
                            result: lash_kernel_value::Type::Timestamp,
                        },
                        lash_kernel_value::ContractId::new("fixture.clock"),
                    ),
                    intents: BTreeSet::new(),
                    controls: TurnControls::none(),
                    mode: OperationMode::Read,
                }
                .into(),
            ),
        },
    )])
}
#[test]
fn l_superset_extra_members_do_not_change_resolution() {
    let one = Namespaces::new(
        BTreeMap::from([(path("typescript"), package())]),
        BTreeSet::new(),
        &|_| false,
    )
    .unwrap();
    let extra = Namespaces::new(
        BTreeMap::from([
            (path("typescript"), package()),
            (path("extra"), BTreeMap::new()),
        ]),
        BTreeSet::new(),
        &|_| false,
    )
    .unwrap();
    assert_eq!(
        one.resolve(&["typescript", "clock"], Spelling::TypeScript),
        extra.resolve(&["typescript", "clock"], Spelling::TypeScript)
    );
}
#[test]
fn l_deferred_attach_only_declared_paths_lower_to_deferred_uses() {
    let names = Namespaces::new(
        BTreeMap::from([(path("typescript"), package())]),
        BTreeSet::from([path("wip")]),
        &|_| false,
    )
    .unwrap();
    assert!(
        matches!(names.resolve(&["wip","sendItem"],Spelling::TypeScript),Resolution::Deferred{operation} if operation.effect_name().as_str()=="wip.send_item")
    );
    assert!(matches!(
        names.resolve(&["typescript", "missing"], Spelling::TypeScript),
        Resolution::Unknown
    ));
    assert!(matches!(
        names.resolve(&["other", "sendItem"], Spelling::TypeScript),
        Resolution::Unknown
    ));
}
#[test]
fn l_deferred_attach_enumerated_and_deferred_paths_cannot_overlap() {
    assert!(
        Namespaces::new(
            BTreeMap::from([(path("typescript"), package())]),
            BTreeSet::from([path("typescript.wip")]),
            &|_| false
        )
        .is_err()
    );
}
