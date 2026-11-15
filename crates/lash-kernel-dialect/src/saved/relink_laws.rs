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
fn relink_keeps_the_function_and_takes_the_new_link() {
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
    let callable = CallableId {
        link: before.identity(),
        function,
    };
    let document = lash_kernel_doc::IdentifiedDocument::new(document);
    let relinked = relink_saved(&Name::new("keep"), &document, callable, &after).unwrap();
    assert_eq!(
        relinked,
        CallableId {
            link: after.identity(),
            function,
        }
    );
}
