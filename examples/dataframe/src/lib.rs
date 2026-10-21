//! A host-owned dataframe package. No callback enters the native runner.
pub mod evaluator;
pub mod host;
pub mod runner;

use lash::package::{HeldPackages, Package, PackageSources};
use std::sync::Arc;

pub const TYPESCRIPT: &str = include_str!("../wordcount.ts");
pub const PYTHON: &str = include_str!("../wordcount.py");

pub fn packages() -> Result<Arc<HeldPackages>, String> {
    let built = lash::package::lash_packages().map_err(|e| e.to_string())?;
    let artifacts = built
        .artifacts()
        .values()
        .map(|p| p.as_ref().clone())
        .collect::<Vec<_>>();
    let kernel = artifacts
        .iter()
        .find(|p| p.path().to_string() == "kernel")
        .ok_or("missing kernel artifact")?;
    let dataframe = Package::build(
        "dataframe",
        "1.0",
        &PackageSources {
            kernel: include_str!("../dataframe.kernel"),
            surface: include_str!("../dataframe.surface"),
            labels: include_str!("../dataframe.labels.json"),
        },
        &[kernel],
    )
    .map_err(|e| e.to_string())?;
    HeldPackages::install(artifacts.into_iter().chain([dataframe]))
        .map(Arc::new)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
