//! The Python dialect's helpers, as Rust plans compiled from their kernel
//! bodies (FIG-6678).
//!
//! The helpers' bodies are their definitions. The helper compiler
//! (`lash-ext-helper-compiler`) compiles each closed body to a plan the machine
//! runs where a native would answer, through the machine's own primitives;
//! the body stays what forced-fallback layouts and a restored frame run.
//! The plans are generated sources, checked in beside their manifest
//! (`generated/plans.tsv`), and a test here fails when they are not what the
//! compiler gives for the current bodies.
//!
//! The dialect crate defines the helpers and knows nothing of this crate;
//! an embedder installs each plan beside the helper its build registered
//! ([`implement`]).

use lash_kernel_doc::FunctionRegistry;
use lash_kernel_doc::RegistryError;
use lash_vocabulary::names::FunctionId;

#[rustfmt::skip]
#[path = "generated/plans.rs"]
mod plans;

pub use plans::PLANS;

/// Why [`implement`] could not install the plans.
#[derive(Debug, thiserror::Error)]
pub enum ImplementError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

/// Installs each plan of [`PLANS`] beside the helper of `helpers` it was
/// compiled from. A plan of a helper the registry does not hold is not
/// installed. Closed kernel callee plans are installed as dependencies;
/// an already installed kernel plan is shared by both dialects.
///
/// # Errors
///
/// [`ImplementError::Registry`] when a helper already has a plan or a
/// closed callee has neither a total native nor a body plan.
pub fn implement(
    registry: &mut FunctionRegistry,
    helpers: impl IntoIterator<Item = FunctionId>,
) -> Result<(), ImplementError> {
    let helpers: std::collections::BTreeSet<FunctionId> = helpers.into_iter().collect();
    let plans: Vec<&'static lash_kernel_doc::plan::HelperPlan> = PLANS
        .iter()
        .copied()
        .filter(|plan| {
            helpers.contains(&plan.function)
                || !plan.name.starts_with("py.")
                    && !plan.name.starts_with("ts.")
                    && registry
                        .get(&plan.function)
                        .is_some_and(|registered| registered.plan().is_none())
        })
        .collect();
    registry.install_plans(&plans)?;
    Ok(())
}
