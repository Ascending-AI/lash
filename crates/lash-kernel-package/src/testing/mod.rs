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
