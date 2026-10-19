//! What an embedding adds to held code: natives and plans, by identity.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::plan::HelperPlan;
use lash_kernel_doc::{FunctionId, NativeFunction};

/// The natives and generated plans an embedding supplies for held code,
/// each by the identity of the function it implements. Not data: an
/// embedding's own, never stored or sent.
#[derive(Clone, Default)]
pub struct PackageInstall {
    pub natives: BTreeMap<FunctionId, Arc<dyn NativeFunction>>,
    pub plans: Vec<&'static HelperPlan>,
}

impl std::fmt::Debug for PackageInstall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackageInstall")
            .field("natives", &self.natives.keys().collect::<Vec<_>>())
            .field("plans", &self.plans.len())
            .finish()
    }
}

impl PackageInstall {
    /// Adds `native` for `function`.
    #[must_use]
    pub fn native(mut self, function: FunctionId, native: Arc<dyn NativeFunction>) -> Self {
        self.natives.insert(function, native);
        self
    }

    /// Adds every native and plan of `other`.
    pub fn extend(&mut self, other: Self) {
        self.natives.extend(other.natives);
        self.plans.extend(other.plans);
    }
}
