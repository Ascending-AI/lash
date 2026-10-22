//! The helper Documents and native declarations shared by conformance and loop audit.
#![allow(clippy::unwrap_used, reason = "fixture construction must succeed")]
use lash_ext_helper_compiler::library::{Dialect, Library};
use lash_kernel_doc::{Document, FunctionRegistry, NumberPolicy};
use std::sync::Arc;

pub(crate) fn installed() -> (Arc<Document>, FunctionRegistry) {
    let ts = Library::of(Dialect::TypeScript).unwrap();
    let py = Library::of(Dialect::Python).unwrap();
    let mut registry = ts.registry;
    let mut pending: Vec<_> = py
        .registry
        .iter()
        .filter(|(id, _)| registry.get(id).is_none())
        .map(|(_, function)| {
            (
                function.definition().as_ref().clone(),
                function.native().cloned(),
            )
        })
        .collect();
    while !pending.is_empty() {
        let before = pending.len();
        pending.retain(|(definition, native)| {
            registry
                .register(definition.clone(), native.clone())
                .is_err()
        });
        assert!(
            pending.len() < before,
            "helper dependencies register in order"
        );
    }
    lash_ext_ts_core::implement(&mut registry, ts.helpers).unwrap();
    lash_ext_py_core::implement(&mut registry, py.helpers).unwrap();
    let mut document = Document::new(NumberPolicy::BySpelling, vec![]);
    document.main = None;
    document.functions = registry
        .iter()
        .map(|(id, function)| (*id, function.definition().as_ref().clone()))
        .collect();
    document.requires.natives = document
        .functions
        .iter()
        .filter(|(_, definition)| {
            matches!(
                definition.implementation,
                lash_kernel_doc::Implementation::Native { .. }
            )
        })
        .map(|(id, _)| *id)
        .collect();
    (Arc::new(document), registry)
}
