//! Authored definitions and import inspection for dialect unit laws.
use lash_kernel_dialect::Library as _;
use lash_vocabulary::{EffectName, ImportKind, Name, QualifiedOperation};
use std::collections::BTreeSet;
pub(super) fn exported<'a>(
    document: &'a lash_kernel_doc::Document,
    name: &Name,
) -> &'a lash_kernel_doc::FunctionExport {
    let lash_kernel_doc::Export::Function(function) = &document.exports[name] else {
        panic!("function export")
    };
    function
}

/// Linked semantic oracles are authored as definitions; only the stated roots
/// are callable by an embedding host.
pub(super) fn definitions(
    source: &str,
    roots: &[&str],
    library: &lash_kernel_dialect::NamedLibrary,
) -> lash_kernel_doc::Document {
    let mut names = library.clone();
    let definitions = lash_kernel_dialect::define_functions(source, &mut names)
        .expect("the authored fixture is valid");
    let mut document =
        lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::BySpelling, vec![]);
    document.main = None;
    for definition in definitions {
        if let Some(body) = definition.body() {
            document.imports.extend(body.imports.clone());
        }
        document.functions.insert(definition.identity(), definition);
    }
    for root in roots {
        let id = names.resolve(root).expect("fixture root");
        let definition = &document.functions[&id];
        document.exports.insert(
            Name::new(*root),
            lash_kernel_doc::Export::Function(lash_kernel_doc::FunctionExport {
                function: id,
                signature: definition.signature.clone(),
                convention: lash_kernel_doc::StartConvention::Direct,
                asynchronous: false,
                rest: false,
            }),
        );
    }
    let used: BTreeSet<_> = document
        .functions
        .values()
        .flat_map(|definition| {
            definition
                .body()
                .into_iter()
                .flat_map(|body| lash_kernel_doc::Node::Block(&body.block).functions_within())
        })
        .collect();
    lash_kernel_dialect::authoring::include_library_dependencies(&mut document, used, &names)
        .expect("library references");
    document
}

pub(super) fn main(
    source: &str,
    library: &lash_kernel_dialect::NamedLibrary,
) -> lash_kernel_doc::Document {
    let mut document = definitions(source, &["law.main"], library);
    let lash_kernel_doc::Export::Function(root) = document
        .exports
        .remove(&Name::new("law.main"))
        .expect("the authored fixture is valid")
    else {
        panic!("fixture root")
    };
    document.main = Some(
        document.functions[&root.function]
            .body()
            .expect("the authored fixture is valid")
            .block
            .clone(),
    );
    document.functions.remove(&root.function);
    document
}

/// The exact build library reference selected by lowering.
pub(super) fn helper_reference(
    document: &lash_kernel_doc::Document,
    helper: &str,
) -> Option<lash_vocabulary::FunctionId> {
    let id = super::library().resolve(helper)?;
    document.library.contains(&id).then_some(id)
}

/// An operation's diagnostic name identifies its declaration, never its call syntax.
pub(super) fn operation_slot(
    document: &lash_kernel_doc::Document,
    operation: &str,
) -> Option<lash_vocabulary::ImportSlot> {
    let name = QualifiedOperation::from_effect(
        &EffectName::new(operation).expect("the authored fixture is valid"),
    )
    .expect("operation name");
    document.imports.iter().find_map(|(slot, import)| {
        (import.name.package == name.package
            && import.name.member == name.name
            && import.name.type_member.is_none()
            && matches!(import.kind, ImportKind::Operation { .. }))
        .then_some(*slot)
    })
}
