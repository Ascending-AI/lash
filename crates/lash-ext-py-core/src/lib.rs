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

use lash_kernel_doc::{FunctionId, FunctionRegistry, RegistryError};

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
/// installed.
///
/// # Errors
///
/// [`ImplementError::Registry`] when a helper has a native already, or a
/// plan calls a body the registry answers with neither a plan nor a native.
pub fn implement(
    registry: &mut FunctionRegistry,
    helpers: impl IntoIterator<Item = FunctionId>,
) -> Result<(), ImplementError> {
    let helpers: std::collections::BTreeSet<FunctionId> = helpers.into_iter().collect();
    let plans: Vec<&'static lash_kernel_doc::plan::HelperPlan> = PLANS
        .iter()
        .copied()
        .filter(|plan| helpers.contains(&plan.function))
        .collect();
    registry.install_plans(&plans)?;
    Ok(())
}
