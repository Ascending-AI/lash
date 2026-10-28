//! L-ALIAS: fresh fulfilment changes the defining Link, never executable or captured identity.
use super::*;
use lash_kernel_doc::{NumberPolicy, parse_definition};

use {
    lash_kernel_value::CallableId, lash_kernel_value::names::ImportSlot,
    lash_kernel_value::names::Name, lash_kernel_value::operations::ContractId,
    lash_kernel_value::operations::OperationRequirement, lash_kernel_value::package::Fill,
    lash_kernel_value::package::Link,
};
#[test]
fn relink_preserves_document_function_environment_and_nested_membership() {
    let definition = parse_definition(
        "function app.keep() -> Any\nkernel 1\nmetering statements\nbody { return null }",
    )
    .unwrap();
    let function = definition.identity();
    let mut document = Document::new(NumberPolicy::Float, vec![]);
    document.functions.insert(function, definition);
    let signature = lash_kernel_value::types::Signature {
        params: vec![],
        result: lash_kernel_value::types::Type::Json,
    };
    document.imports.insert(
        ImportSlot(0),
        lash_kernel_value::names::Import {
            name: lash_kernel_value::names::QualifiedName {
                package: lash_kernel_value::names::Vec1::new(
                    lash_kernel_value::names::Segment::new("app").unwrap(),
                ),
                member: lash_kernel_value::names::Segment::new("fetch").unwrap(),
                type_member: None,
            },
            kind: lash_kernel_value::names::ImportKind::Operation {
                signature: signature.clone(),
            },
        },
    );
    let pinned = |contract| Link {
        helper_fills: Default::default(),
        document: document.identity(),
        fills: BTreeMap::from([(
            ImportSlot(0),
            Fill::Operation(OperationRequirement {
                signature: signature.clone(),
                contract: ContractId::new(contract),
            }),
        )]),
    };
    let before = pinned("c1");
    let after = pinned("c2");
    let environment = EntityId {
        family: FamilyId::PRIMARY,
        ordinal: 1,
    };
    let saved = SavedFunction {
        name: Name::new("keep"),
        link: before.identity(),
        callable: CallableId {
            link: before.identity(),
            function,
        },
        environment,
        document: lash_kernel_doc::IdentifiedDocument::new(document.clone()),
        captures: BTreeMap::new(),
        closure: PageRef {
            id: PageId([1; 32]),
            grant: PageGrant::NONE,
        },
        references: BTreeSet::from([EntityId {
            family: FamilyId::PRIMARY,
            ordinal: 2,
        }]),
        written: None,
        token: None,
        held: None,
    };
    let relinked = relink_saved(&saved, &after).unwrap();
    assert_eq!(relinked.document.document(), &document);
    assert_eq!(relinked.callable.function, function);
    assert_eq!(relinked.link, after.identity());
    assert_eq!(relinked.callable.link, after.identity());
    assert_eq!(relinked.environment, saved.environment);
    assert_eq!(relinked.references, saved.references);
    assert_eq!(relinked.closure, saved.closure);
    assert_eq!(saved.link, before.identity());
}
