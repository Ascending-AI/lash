//! Linked documents and ordinary operation scripts for authored dialect laws.
use lash_kernel_dialect::{
    EffectControl, NamespaceKind, NamespaceMember, Namespaces, OperationSignature,
};
use lash_vocabulary::package::{Fill, Link, MemberKind, Visibility};
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
/// The fixture's exact Link DAG, shared by execution and display laws.
pub(super) fn content(
    document: &lash_kernel_doc::Document,
) -> (lash_kernel_package::LinkedContent, lash_vocabulary::LinkId) {
    let release = lash_vm_releases::helper_release()
        .unwrap_or_else(|error| panic!("fixture helper release: {error}"));
    HELPER_SNAPSHOTS.with(|copies| copies.set(copies.get() + 1));
    let mut content = release.content.clone();
    let link = link(document, &content);
    let root = link.identity();
    content.documents.insert(link.document, document.clone());
    content.links.insert(root, link);
    (content, root)
}

fn link(
    document: &lash_kernel_doc::Document,
    content: &lash_kernel_package::LinkedContent,
) -> Link {
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
                    .unwrap_or_else(|| panic!("fixture helper export {} is absent", import.name));
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
    Link {
        document: document.identity(),
        fills,
    }
}
pub(super) fn program(
    document: Arc<lash_kernel_doc::Document>,
    library: lash_kernel_vm::PreparedLibrary,
) -> lash_kernel_vm::Program {
    let document_id = document.identity();
    FIXTURE_CONTENT.with(|fixture| {
        let mut retained = fixture.borrow_mut();
        let release = lash_vm_releases::helper_release()
            .unwrap_or_else(|error| panic!("fixture helpers: {error}"));
        let link = link(&document, &release.content);
        let root = link.identity();
        let mut scope = RootContent {
            previous_document: if retained.is_none() {
                release.content.documents.get(&document_id).cloned()
            } else {
                None
            },
            previous_link: if retained.is_none() {
                release.content.links.get(&root).cloned()
            } else {
                None
            },
            document: document_id,
            link: root,
            content: &mut retained,
        };
        if let Some(content) = scope.content.as_mut() {
            scope.previous_document = content
                .documents
                .insert(document_id, document.as_ref().clone());
            scope.previous_link = content.links.insert(root, link);
        } else {
            // The first cell owns the graph's initial membership too. Later
            // cells reuse the same published graph after its root is removed.
            *scope.content = Some(content(&document).0);
        }
        let content = scope
            .content
            .as_ref()
            .unwrap_or_else(|| unreachable!("fixture graph initialized"));

        let reach = content
            .inspect(root, &lash_kernel_package::ReachSelection::AllExports)
            .unwrap_or_else(|error| panic!("fixture executable reach: {error}"));
        // Reuse the immutable helper allocations across roots; each root keeps its
        // own Document and Link, and compilation takes exactly the inspected reach.
        static HELPERS: std::sync::OnceLock<
            BTreeMap<lash_vocabulary::DocumentId, Arc<lash_kernel_doc::Document>>,
        > = std::sync::OnceLock::new();
        let helpers = HELPERS.get_or_init(|| {
            lash_vm_releases::helper_release()
                .unwrap_or_else(|error| panic!("fixture helpers: {error}"))
                .content
                .documents
                .iter()
                .map(|(id, document)| (*id, Arc::new(document.clone())))
                .collect()
        });
        let mut program = lash_kernel_vm::Program::prepare(
            root,
            content
                .documents
                .iter()
                .map(|(id, d)| {
                    (
                        *id,
                        if *id == document_id {
                            Arc::clone(&document)
                        } else {
                            helpers
                                .get(id)
                                .cloned()
                                .unwrap_or_else(|| Arc::new(d.clone()))
                        },
                    )
                })
                .collect(),
            &content.links,
            super::pages::CLOSURE,
            library,
        )
        .unwrap_or_else(|error| panic!("linked fixture admission: {error:?}"));
        program.functions = reach.functions.into_values().flatten().collect();
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
    })
}

// The fixture host keeps its published helper graph for the thread's cases.
// A cell is a scoped addition, not a copy of that graph. Program documents
// retain the shared immutable helper allocations after the scope ends.
thread_local! {
    static FIXTURE_CONTENT: RefCell<Option<lash_kernel_package::LinkedContent>> = const { RefCell::new(None) };
    pub(super) static HELPER_SNAPSHOTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Own the temporary graph membership before inspection or admission can fail.
struct RootContent<'a> {
    content: &'a mut Option<lash_kernel_package::LinkedContent>,
    document: lash_vocabulary::DocumentId,
    link: lash_vocabulary::LinkId,
    previous_document: Option<lash_kernel_doc::Document>,
    previous_link: Option<Link>,
}

impl Drop for RootContent<'_> {
    fn drop(&mut self) {
        let Some(content) = self.content.as_mut() else {
            return;
        };
        content.documents.remove(&self.document);
        content.links.remove(&self.link);
        if let Some(document) = self.previous_document.take() {
            content.documents.insert(self.document, document);
        }
        if let Some(link) = self.previous_link.take() {
            content.links.insert(self.link, link);
        }
    }
}
