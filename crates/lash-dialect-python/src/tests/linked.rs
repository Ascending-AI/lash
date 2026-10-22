//! Linked documents and ordinary operation scripts for authored dialect laws.
use lash_kernel_dialect::{
    EffectControl, NamespaceKind, NamespaceMember, Namespaces, OperationSignature,
};
use lash_vocabulary::package::{Fill, Link, MemberKind, Visibility};
use lash_vocabulary::{
    EffectName, ImportKind, Name, OperationMode, OperationUse, QualifiedName, QualifiedOperation,
    Signature, TurnControls,
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
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
            accepted = accepted.with_finish(lash_vocabulary::JsonSchema::any());
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
    let release = lash_vm_releases::helper_release().expect("helper release");
    let mut content = release.content.clone();
    let mut fills = BTreeMap::new();
    for (slot, import) in &document.imports {
        let fill = match &import.kind {
            ImportKind::Operation { signature } => Fill::Operation(
                lash_kernel_dialect::authoring::service_requirement(&import.name).unwrap_or_else(
                    || lash_vocabulary::OperationRequirement {
                        signature: signature.clone(),
                        contract: lash_vocabulary::ContractId::new("fixture"),
                    },
                ),
            ),
            _ => {
                let member = content
                    .packages
                    .values()
                    .find(|package| package.path == import.name.package)
                    .and_then(|package| package.members.get(&import.name.member))
                    .expect("helper package export");
                let MemberKind::Export { link, export } = &member.kind else {
                    panic!("code export")
                };
                let mut export = export.clone();
                export.member = import.name.type_member.clone();
                Fill::Export {
                    link: *link,
                    export,
                }
            }
        };
        fills.insert(*slot, fill);
    }
    let id = document.identity();
    content.documents.insert(id, (*document).clone());
    let link = Link {
        document: id,
        fills,
    };
    let root = link.identity();
    content.links.insert(root, link);
    let program = lash_kernel_vm::Program::prepare(
        root,
        content
            .documents
            .into_iter()
            .map(|(id, d)| (id, Arc::new(d)))
            .collect(),
        &content.links,
        super::pages::CLOSURE,
        library,
    )
    .expect("linked fixture");
    OPERATIONS.with(|operations| {
        let mut operations = operations.borrow_mut();
        for (link, instance) in &program.instances {
            for (slot, import) in &program.documents[&instance.document].imports {
                if matches!(import.kind, ImportKind::Operation { .. }) {
                    operations.insert(
                        OperationUse {
                            link: *link,
                            slot: *slot,
                        },
                        import.name.clone(),
                    );
                }
            }
        }
    });
    program
}
pub(super) fn modes(program: &lash_kernel_vm::Program) -> lash_kernel_vm::EffectModes {
    program
        .instances
        .iter()
        .flat_map(|(link, instance)| {
            program.documents[&instance.document]
                .imports
                .iter()
                .filter(|(_, import)| matches!(import.kind, ImportKind::Operation { .. }))
                .map(move |(slot, import)| {
                    let read = matches!(
                        import.name.package.to_string().as_str(),
                        "typescript" | "python"
                    ) && matches!(import.name.member.as_str(), "clock" | "random");
                    (
                        OperationUse {
                            link: *link,
                            slot: *slot,
                        },
                        if read {
                            OperationMode::Read
                        } else {
                            OperationMode::Waited
                        },
                    )
                })
        })
        .collect()
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

/// The document-local slot of exactly this code helper, when it is imported.
pub(super) fn helper_slot(
    document: &lash_kernel_doc::Document,
    helper: &str,
) -> Option<lash_vocabulary::ImportSlot> {
    let name = lash_kernel_dialect::authoring::helper_name(
        &lash_vocabulary::FunctionName::new(helper).expect("helper name"),
    );
    document.imports.iter().find_map(|(slot, import)| {
        (import.name == name && matches!(import.kind, ImportKind::Code { .. })).then_some(*slot)
    })
}

/// An operation's diagnostic name identifies its declaration, never its call syntax.
pub(super) fn operation_slot(
    document: &lash_kernel_doc::Document,
    operation: &str,
) -> Option<lash_vocabulary::ImportSlot> {
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
