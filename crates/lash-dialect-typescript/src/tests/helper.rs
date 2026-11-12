//! A test library that resolves one helper name to another identity.

use lash_kernel_dialect::Library as _;

/// A library whose `name` resolves to `id` instead of its own helper.
pub(crate) struct Helper<'a> {
    pub(crate) library: &'a lash_kernel_dialect::NamedLibrary,
    pub(crate) name: &'static str,
    pub(crate) id: Option<lash_kernel_value::FunctionId>,
}

impl lash_kernel_doc::FunctionCatalog for Helper<'_> {
    fn named_helper(&self, name: &str) -> Option<lash_kernel_value::FunctionId> {
        self.resolve(name)
    }
    fn projection_helper(
        &self,
        role: lash_kernel_doc::ProjectionRole,
    ) -> Option<lash_kernel_value::FunctionId> {
        self.library.projection_helper(role)
    }
    fn helper_library(&self) -> Option<&lash_kernel_doc::BuildHelpers> {
        self.library.helper_library()
    }
    fn definition(
        &self,
        function: &lash_kernel_value::FunctionId,
    ) -> Option<&lash_kernel_doc::FunctionDefinition> {
        self.library.definition(function)
    }
    fn registered(
        &self,
        function: &lash_kernel_value::FunctionId,
    ) -> Option<&lash_kernel_doc::RegisteredFunction> {
        self.library.registered(function)
    }
    fn header(
        &self,
        function: &lash_kernel_value::FunctionId,
    ) -> Option<std::sync::Arc<lash_kernel_doc::FunctionHeader>> {
        self.library.header(function)
    }
    fn registration(
        &self,
        definition: &lash_kernel_doc::FunctionDefinition,
    ) -> Option<&lash_kernel_doc::RegisteredFunction> {
        self.library.registration(definition)
    }
}

impl lash_kernel_dialect::Library for Helper<'_> {
    fn resolve(&self, name: &str) -> Option<lash_kernel_value::FunctionId> {
        if name == self.name {
            return self.id;
        }
        self.library.resolve(name)
    }
}
