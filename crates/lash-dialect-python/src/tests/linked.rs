//! Linked documents and ordinary operation scripts for authored dialect laws.
use lash_kernel_dialect::{
    EffectControl, NamespaceKind, NamespaceMember, Namespaces, OperationSignature,
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use {
    lash_kernel_value::EffectName, lash_kernel_value::ImportKind, lash_kernel_value::Name,
    lash_kernel_value::OperationMode, lash_kernel_value::OperationUse,
    lash_kernel_value::QualifiedName, lash_kernel_value::QualifiedOperation,
    lash_kernel_value::Signature, lash_kernel_value::TurnControls,
};
use {
    lash_kernel_value::package::Fill, lash_kernel_value::package::Link,
    lash_kernel_value::package::Visibility,
};

pub(super) fn namespaces(
    effects: &BTreeMap<EffectName, Signature>,
    controls: &BTreeMap<EffectName, BTreeSet<EffectControl>>,
) -> Result<Namespaces, lash_kernel_dialect::NamespaceRefusal> {
    let mut packages = BTreeMap::new();
    for (name, signature) in effects {
        let operation = QualifiedOperation::from_effect(name).expect("fixture operation");
        let flags = controls.get(name);
        let mut accepted = TurnControls::none();
        if flags.is_some_and(|flags| flags.contains(&EffectControl::Finish)) {
            accepted = accepted.with_finish(lash_kernel_value::JsonSchema::any());
        }
        if flags.is_some_and(|flags| flags.contains(&EffectControl::SwitchAgentFrame)) {
            accepted = accepted.with_switch_agent_frame();
        }
        packages
            .entry(operation.package)
            .or_insert_with(BTreeMap::new)
            .insert(
                operation.name,
                NamespaceMember {
                    visibility: Visibility::Public,
                    kind: NamespaceKind::Operation(OperationSignature {
                        signature: signature.clone(),
                        controls: accepted,
                        mode: OperationMode::Waited,
                    }),
                },
            );
    }
    Namespaces::new(packages, BTreeSet::new(), &|_| false)
}

thread_local! {
    // Test-owned immutable Link content, used only to name scripted observations.
    static OPERATIONS: RefCell<BTreeMap<OperationUse, QualifiedName>> = const { RefCell::new(BTreeMap::new()) };
}
pub(super) fn operation(use_: &OperationUse) -> EffectName {
    OPERATIONS.with(|operations| {
        let operations = operations.borrow();
        let name = operations
            .get(use_)
            .expect("scripted operation belongs to a prepared Link");
        QualifiedOperation {
            package: name.package.clone(),
            name: name.member.clone(),
        }
        .effect_name()
    })
}
pub(super) fn program(
    document: Arc<lash_kernel_doc::Document>,
    library: lash_kernel_vm::PreparedLibrary,
) -> lash_kernel_vm::Program {
    let mut content = lash_kernel_package::LinkedContent::default();
    let mut fills = BTreeMap::new();
    for (slot, import) in &document.imports {
        let fill = match &import.kind {
            ImportKind::Operation { signature } => Fill::Operation(
                lash_ext_py_helpers::package::helper_operations()
                    .iter()
                    .find(|operation| operation.name == import.name)
                    .map(|operation| operation.requirement.clone())
                    .unwrap_or_else(|| lash_kernel_value::OperationRequirement {
                        signature: signature.clone(),
                        contract: lash_kernel_value::ContractId::new("fixture"),
                    }),
            ),
            _ => panic!("ordinary code fixtures supply their own Link graph"),
        };
        fills.insert(*slot, fill);
    }
    let id = document.identity();
    content
        .documents
        .insert(id, std::sync::Arc::clone(&document));
    let link = Link {
        helper_fills: lash_kernel_doc::helper_imports(&document, library.helpers().as_ref())
            .unwrap_or_else(|error| panic!("scripted helper declarations: {error}"))
            .into_iter()
            .map(|(id, import)| {
                let operation = lash_kernel_services::TYPESCRIPT
                    .iter()
                    .chain(lash_kernel_services::PYTHON.iter())
                    .find(|operation| operation.name == import.name)
                    .unwrap_or_else(|| {
                        panic!("scripted helper service {} is not declared", import.name)
                    });
                (
                    id,
                    lash_kernel_value::HelperFill {
                        import,
                        fill: Fill::Operation(operation.requirement.clone()),
                    },
                )
            })
            .collect(),
        document: id,
        fills,
    };
    let root = link.identity();
    content.links.insert(root, link);
    let program = lash_kernel_vm::Program::prepare(
        root,
        content.documents.into_iter().collect(),
        &content.links,
        super::pages::CLOSURE,
        library,
        lash_kernel_doc::Selection::Fresh,
    )
    .expect("linked fixture");
    OPERATIONS.with(|operations| {
        let mut operations = operations.borrow_mut();
        for (link, instance) in &program.instances {
            for (id, import) in &instance.helper_operations {
                operations.insert(
                    OperationUse {
                        link: *link,
                        slot: lash_kernel_value::OperationSlot::Helper(*id),
                    },
                    import.name.clone(),
                );
            }
            for (slot, import) in &program.documents[&instance.document].imports {
                if matches!(import.kind, ImportKind::Operation { .. }) {
                    operations.insert(
                        OperationUse {
                            link: *link,
                            slot: (*slot).into(),
                        },
                        import.name.clone(),
                    );
                }
            }
        }
    });
    program
}
pub(crate) fn modes(program: &lash_kernel_vm::Program) -> lash_kernel_vm::EffectModes {
    let mut modes = std::collections::BTreeMap::new();
    for (link, instance) in &program.instances {
        for (slot, import) in program.documents[&instance.document]
            .imports
            .iter()
            .map(|(slot, import)| (lash_kernel_value::OperationSlot::Import(*slot), import))
            .chain(
                instance
                    .helper_operations
                    .iter()
                    .map(|(id, import)| (lash_kernel_value::OperationSlot::Helper(*id), import)),
            )
        {
            if matches!(import.kind, ImportKind::Operation { .. }) {
                let read = matches!(
                    import.name.package.to_string().as_str(),
                    "typescript" | "python"
                ) && matches!(import.name.member.as_str(), "clock" | "random");
                modes.insert(
                    OperationUse { link: *link, slot },
                    if read {
                        OperationMode::Read
                    } else {
                        OperationMode::Waited
                    },
                );
            }
        }
    }
    modes
}
pub(super) fn read(
    use_: &OperationUse,
    bits: u64,
) -> Result<Result<lash_kernel_doc::Datum, lash_kernel_doc::ErrorDatum>, lash_kernel_vm::HostFault>
{
    use lash_kernel_doc::{Datum, Integer, Timestamp};

    let name = operation(use_);
    Ok(Ok(if name.as_str().ends_with(".clock") {
        Datum::Timestamp(Timestamp {
            nanoseconds: Integer::from(0),
        })
    } else if name.as_str().ends_with(".random") {
        Datum::Bytes(lash_kernel_doc::Bytes::new(bits.to_be_bytes().as_slice()))
    } else {
        return Err(lash_kernel_vm::HostFault::new(format!(
            "unexpected read {name}"
        )));
    }))
}

pub(super) fn exported<'a>(
    document: &'a lash_kernel_doc::Document,
    name: &Name,
) -> &'a lash_kernel_doc::FunctionExport {
    let lash_kernel_doc::Export::Function(function) = &document.exports[name] else {
        panic!("function export")
    };
    function
}

/// The exact build library reference selected by lowering.
pub(super) fn helper_reference(
    document: &lash_kernel_doc::Document,
    helper: &str,
) -> Option<lash_kernel_value::FunctionId> {
    use lash_kernel_dialect::Library as _;
    let id = super::machine::library().resolve(helper)?;
    document.library.contains(&id).then_some(id)
}

/// An operation's diagnostic name identifies its declaration, never its call syntax.
pub(super) fn operation_slot(
    document: &lash_kernel_doc::Document,
    operation: &str,
) -> Option<lash_kernel_value::ImportSlot> {
    let name = QualifiedOperation::from_effect(&EffectName::new(operation).unwrap())
        .expect("operation name");
    document.imports.iter().find_map(|(slot, import)| {
        (import.name.package == name.package
            && import.name.member == name.name
            && import.name.type_member.is_none()
            && matches!(import.kind, ImportKind::Operation { .. }))
        .then_some(*slot)
    })
}
