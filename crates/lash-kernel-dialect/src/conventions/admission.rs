//! The source inverse is checked at the atomic publication boundary.
use lash_kernel_doc::{self as k, FunctionCatalog};
use lash_kernel_value::FunctionId;
use std::collections::BTreeMap;

pub(super) struct CatalogLibrary<'a> {
    catalog: &'a dyn FunctionCatalog,
    document: &'a k::Document,
    pinned: BTreeMap<String, Option<FunctionId>>,
}
impl<'a> CatalogLibrary<'a> {
    pub(super) fn new(catalog: &'a dyn FunctionCatalog, document: &'a k::Document) -> Self {
        let mut pinned = BTreeMap::new();
        let listed = document
            .library
            .iter()
            .copied()
            .chain(document.requires.natives.iter().copied())
            .chain(
                document
                    .functions
                    .values()
                    .filter_map(|function| function.body())
                    .flat_map(|body| body.functions.keys().copied()),
            );
        for id in listed {
            let Some(header) = catalog.header(&id) else {
                continue;
            };
            pinned
                .entry(header.name.to_string())
                .and_modify(|found| {
                    if *found != Some(id) {
                        *found = None;
                    }
                })
                .or_insert(Some(id));
        }
        Self {
            catalog,
            document,
            pinned,
        }
    }
}
impl FunctionCatalog for CatalogLibrary<'_> {
    fn named_helper(&self, name: &str) -> Option<FunctionId> {
        crate::Library::resolve(self, name)
    }
    fn definition(&self, id: &FunctionId) -> Option<&k::FunctionDefinition> {
        self.document
            .functions
            .get(id)
            .or_else(|| self.catalog.definition(id))
    }
    fn registered(&self, id: &FunctionId) -> Option<&k::RegisteredFunction> {
        self.catalog.registered(id)
    }
    fn header(&self, id: &FunctionId) -> Option<std::sync::Arc<k::FunctionHeader>> {
        self.document
            .functions
            .get(id)
            .map(|definition| std::sync::Arc::new(k::FunctionHeader::of(definition)))
            .or_else(|| self.catalog.header(id))
    }
    fn helper_library(&self) -> Option<&k::BuildHelpers> {
        self.catalog.helper_library()
    }
    fn projection_helper(&self, role: k::ProjectionRole) -> Option<FunctionId> {
        let mut found = self
            .pinned
            .values()
            .filter_map(|function| *function)
            .filter(|id| {
                self.catalog
                    .header(id)
                    .is_some_and(|header| header.projection == role)
            });
        match found.next() {
            Some(id) => found.next().is_none().then_some(id),
            None => self.catalog.projection_helper(role),
        }
    }
    fn registration(&self, definition: &k::FunctionDefinition) -> Option<&k::RegisteredFunction> {
        self.catalog.registration(definition)
    }
}
impl crate::Library for CatalogLibrary<'_> {
    fn resolve(&self, name: &str) -> Option<FunctionId> {
        // Retained releases may expose several definitions under one name.
        // Source recovery must use the exact helper this document declares.
        match self.pinned.get(name) {
            Some(function) => *function,
            None => self.catalog.named_helper(name),
        }
    }
}

/// Lower the generated source to the document admission will verify and publish.
///
/// # Errors
/// A namespace or ordinary source-lowering refusal.
pub fn verify_lowering(
    document: &k::Document,
    library: &dyn crate::Library,
    lower: impl FnOnce(&crate::Environment<'_>, crate::Mode<'_>) -> Result<k::Document, String>,
) -> Result<k::Document, crate::PrintError> {
    let namespaces = crate::regions::namespaces(document)?;
    let environment = crate::Environment::new(library, &namespaces);
    let mode = if document.exports.is_empty() {
        crate::Mode::Cell(crate::CellEnvironment::first())
    } else {
        crate::Mode::Module
    };
    let read = lower(&environment, mode).map_err(|detail| crate::PrintError::Unprintable {
        site: lash_kernel_value::sites::Site::new(lash_kernel_value::sites::Unit::Main, vec![]),
        reason: crate::UnprintableReason::Main { detail },
    })?;
    Ok(read)
}
