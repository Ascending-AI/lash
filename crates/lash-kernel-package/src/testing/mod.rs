//! The `dataframe` fixture (FIG-6917): a host package with two value
//! types, a package function, an error and two operations, for the laws of
//! this crate and of the crates that install it.

use crate::{Package, PackageRefusal, PackageSources};

/// The fixture's three source files.
pub const DATAFRAME: PackageSources<'static> = PackageSources {
    kernel: include_str!("dataframe.kernel"),
    surface: include_str!("dataframe.surface"),
    labels: include_str!("dataframe.labels.tsv"),
};

/// The fixture's release label.
pub const RELEASE: &str = "0.3.0";

/// The fixture built against `kernel`, the artifact of lash's kernel
/// package its bodies call into.
///
/// # Errors
///
/// The builder's refusals: none for the fixture as written.
pub fn dataframe(kernel: &Package) -> Result<Package, PackageRefusal> {
    Package::build("dataframe", RELEASE, &DATAFRAME, &[kernel])
}

/// lash's kernel package over the functions `register` gives, as the laws
/// of this crate make it, and the fixture built against it: what the laws
/// of other crates that run the fixture's code build alike (FIG-6916).
///
/// # Errors
///
/// `register`'s refusal, or the builder's.
pub fn kernel_and_dataframe(
    register: &dyn Fn(&mut lash_kernel_doc::FunctionRegistry) -> Result<(), String>,
) -> Result<(Package, Package), PackageRefusal> {
    let path =
        crate::PackagePath::parse("kernel").map_err(|invalid| PackageRefusal::NotCanonical {
            role: crate::NameRole::Path,
            text: invalid.text,
        })?;
    let mut probe = lash_kernel_doc::FunctionRegistry::new();
    register(&mut probe).map_err(|message| PackageRefusal::Registry {
        package: path.clone(),
        message,
    })?;
    let roots: std::collections::BTreeSet<crate::Root> = probe
        .iter()
        .map(|(_, function)| {
            let name = function.definition().name.as_str();
            crate::Root::new(name.split('.').next().unwrap_or(name))
        })
        .collect();
    let kernel = Package::lash(path, "1.0", roots, &[], false, |registry| {
        register(registry)?;
        Ok(registry
            .iter()
            .map(|(_, function)| lash_kernel_doc::FunctionDefinition::clone(function.definition()))
            .collect())
    })?;
    let dataframe = dataframe(&kernel)?;
    Ok((kernel, dataframe))
}
