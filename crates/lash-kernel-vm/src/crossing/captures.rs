//! The catalog capture discovery reads: authored adapter bodies and the library.
use super::*;

/// Capture discovery follows authored adapter bodies as well as the library.
pub(super) struct LinkedCatalog<'a> {
    pub(super) document: &'a lash_kernel_doc::Document,
    pub(super) helpers: &'a lash_kernel_doc::BuildHelpers,
    pub(super) selection: &'a lash_kernel_doc::Selection,
    pub(super) link: LinkId,
}

impl FunctionCatalog for LinkedCatalog<'_> {
    fn definition(
        &self,
        function: &lash_kernel_value::FunctionId,
    ) -> Option<&lash_kernel_doc::FunctionDefinition> {
        self.document.functions.get(function).or_else(|| {
            self.selection
                .admits_helper(
                    &CallableId {
                        link: self.link,
                        function: *function,
                    },
                    &self.document.library,
                    self.helpers,
                )
                .then(|| self.helpers.definition(function))
                .flatten()
        })
    }
}
