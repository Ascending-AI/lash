//! Linked documents and ordinary operation scripts for authored dialect laws.
use lash_kernel_dialect::{
    EffectControl, NamespaceKind, NamespaceMember, Namespaces, OperationSignature,
};
use lash_vocabulary::package::{Fill, Link, Visibility};
use lash_vocabulary::{
    EffectName, ImportKind, OperationMode, OperationUse, QualifiedName, QualifiedOperation,
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
        let operation = QualifiedOperation::from_effect(name)
            .unwrap_or_else(|error| panic!("fixture operation {name}: {error}"));
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
            .unwrap_or_else(|| panic!("scripted operation {use_:?} belongs to no prepared Link"));
        QualifiedOperation {
            package: name.package.clone(),
            name: name.member.clone(),
        }
        .effect_name()
    })
}
/// One authored document and its ordinary operation Link. Helpers are installed
/// independently in the prepared library and contribute no retained Documents.
pub(super) fn content(
    document: &lash_kernel_doc::Document,
) -> (lash_kernel_package::LinkedContent, lash_vocabulary::LinkId) {
    let link = link(document);
    let root = link.identity();
    let mut content = lash_kernel_package::LinkedContent::default();
    content.documents.insert(link.document, document.clone());
    content.links.insert(root, link);
    (content, root)
}
fn link(document: &lash_kernel_doc::Document) -> Link {
    Link {
        document: document.identity(),
        fills: document
            .imports
            .iter()
            .map(|(slot, import)| {
                let ImportKind::Operation { signature } = &import.kind else {
                    panic!("ordinary code fixtures supply their own Link graph")
                };
                (
                    *slot,
                    Fill::Operation(
                        lash_dialect_services::TYPESCRIPT
                            .iter()
                            .chain(lash_dialect_services::PYTHON.iter())
                            .find(|operation| operation.name == import.name)
                            .map(|operation| operation.requirement.clone())
                            .unwrap_or_else(|| lash_vocabulary::OperationRequirement {
                                signature: signature.clone(),
                                contract: lash_vocabulary::ContractId::new("fixture"),
                            }),
                    ),
                )
            })
            .collect(),
    }
}
pub(super) fn program(
    document: Arc<lash_kernel_doc::Document>,
    library: lash_kernel_vm::PreparedLibrary,
) -> lash_kernel_vm::Program {
    let (content, root) = content(&document);
    let mut program = lash_kernel_vm::Program::prepare(
        root,
        BTreeMap::from([(document.identity(), Arc::clone(&document))]),
        &content.links,
        super::pages::CLOSURE,
        library,
        lash_kernel_doc::Selection::Fresh,
    )
    .unwrap_or_else(|error| panic!("linked fixture admission: {error:?}"));
    // A worker/custom helper compiler may install a narrower closed build image.
    program.selection = lash_kernel_doc::Selection::Fresh;
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
