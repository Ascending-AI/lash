//! One binding rule for every generated-source rendering and reading path.

use std::collections::BTreeSet;

#[derive(Default)]
pub(crate) struct BindingScope {
    bound: BTreeSet<lash_kernel_value::Name>,
    pub(crate) types: lash_kernel_dialect::source_types::Types,
}
impl BindingScope {
    pub(crate) fn new(params: &[lash_kernel_value::Name]) -> Self {
        Self {
            bound: params.iter().cloned().collect(),
            ..Default::default()
        }
    }
    /// First binding is Let. An explicit kind records the exceptional cases.
    /// Both sides call this before the RHS, including closure declarations.
    pub(crate) fn bind(
        &mut self,
        name: &lash_kernel_value::Name,
        explicit: Option<bool>,
    ) -> (bool, Option<&'static str>) {
        let inferred = !self.bound.contains(name);
        let declaration = explicit.unwrap_or(inferred);
        self.bound.insert(name.clone());
        (
            declaration,
            (declaration != inferred).then_some(if declaration { "Final" } else { "reassigned" }),
        )
    }
    pub(crate) fn introduce(&mut self, name: &lash_kernel_value::Name) {
        self.bound.insert(name.clone());
        self.types.0.remove(name);
    }
}
