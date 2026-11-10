//! The catalog capture discovery reads: authored adapter bodies and the library.
use super::*;

/// Capture discovery follows authored adapter bodies as well as the library.
pub(super) struct LinkedCatalog<'a> {
    pub(super) document: &'a lash_kernel_doc::Document,
    pub(super) helpers: &'a lash_kernel_doc::BuildHelpers,
}

impl FunctionCatalog for LinkedCatalog<'_> {
    fn definition(
        &self,
        function: &lash_kernel_value::FunctionId,
    ) -> Option<&lash_kernel_doc::FunctionDefinition> {
        self.document.functions.get(function).or_else(|| {
            self.helpers
                .admits(&self.document.library, function)
                .then(|| self.helpers.definition(function))
                .flatten()
        })
    }
}
