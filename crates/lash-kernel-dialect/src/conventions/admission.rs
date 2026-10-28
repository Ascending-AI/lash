//! The source inverse is checked at the atomic publication boundary.
use lash_kernel_doc::{self as k, FunctionCatalog};
use lash_kernel_value::FunctionId;

pub(super) struct CatalogLibrary<'a>(pub &'a dyn FunctionCatalog);
impl FunctionCatalog for CatalogLibrary<'_> {
    fn named_helper(&self, name: &str) -> Option<FunctionId> {
        self.0.named_helper(name)
    }
    fn definition(&self, id: &FunctionId) -> Option<&k::FunctionDefinition> {
        self.0.definition(id)
    }
    fn registered(&self, id: &FunctionId) -> Option<&k::RegisteredFunction> {
        self.0.registered(id)
    }
    fn header(&self, id: &FunctionId) -> Option<std::sync::Arc<k::FunctionHeader>> {
        self.0.header(id)
    }
    fn helper_library(&self) -> Option<&k::BuildHelpers> {
        self.0.helper_library()
    }
    fn projection_helper(&self, role: k::ProjectionRole) -> Option<FunctionId> {
        self.0.projection_helper(role)
    }
    fn registration(&self, definition: &k::FunctionDefinition) -> Option<&k::RegisteredFunction> {
        self.0.registration(definition)
    }
}
impl crate::Library for CatalogLibrary<'_> {
    fn resolve(&self, name: &str) -> Option<FunctionId> {
        self.0.named_helper(name)
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
