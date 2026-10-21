//! L-ALIAS: fresh fulfilment changes the defining Link, never executable or captured identity.
use super::*;
use lash_kernel_doc::{NumberPolicy, parse_definition};

use lash_vocabulary::{
    CallableId,
    names::{ImportSlot, Name},
    operations::{ContractId, OperationRequirement},
    package::{Fill, Link},
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
    let signature = lash_vocabulary::types::Signature {
        params: vec![],
        result: lash_vocabulary::types::Type::Json,
    };
    document.imports.insert(
        ImportSlot(0),
        lash_vocabulary::names::Import {
            name: lash_vocabulary::names::QualifiedName {
                package: lash_vocabulary::names::Vec1::new(
                    lash_vocabulary::names::Segment::new("app").unwrap(),
                ),
                member: lash_vocabulary::names::Segment::new("fetch").unwrap(),
                type_member: None,
            },
            kind: lash_vocabulary::names::ImportKind::Operation {
                signature: signature.clone(),
            },
        },
    );
    let pinned = |contract| Link {
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
        document: document.clone(),
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
    assert_eq!(relinked.document, document);
    assert_eq!(relinked.callable.function, function);
    assert_eq!(relinked.link, after.identity());
    assert_eq!(relinked.callable.link, after.identity());
    assert_eq!(relinked.environment, saved.environment);
    assert_eq!(relinked.references, saved.references);
    assert_eq!(relinked.closure, saved.closure);
    assert_eq!(saved.link, before.identity());
}
