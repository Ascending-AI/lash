//! Immutable build-owned helper code and native declarations, addressed by exact FunctionId.

use std::sync::{Arc, OnceLock};

use lash_kernel_doc::{BuildHelpers, FunctionRegistry};

use lash_kernel_value::names::*;
use std::collections::BTreeMap;

/// The TypeScript dialect name recorded by sessions and runs.
pub const TYPESCRIPT: &str = "typescript";
/// The Python dialect name recorded by sessions and runs.
pub const PYTHON: &str = "python";
/// Dialects whose packages the build may include.
pub const SHIPPED_DIALECTS: &[&str] = &[TYPESCRIPT, PYTHON];

#[cfg(test)]
mod tests;

/// One build-generated release image and its native constructor membership.
#[derive(Clone, Copy, Debug)]
pub struct BuiltPackage {
    /// The package's path.
    pub path: &'static str,
    /// Its code, validated against the packages it requires.
    pub image: &'static [u8],
    /// This build's native implementations, sorted by function identity.
    pub natives: &'static [(lash_kernel_value::names::FunctionId, &'static str)],
}

impl BuiltPackage {
    /// The identities and local names declared by this function's body.
    /// Constructor tables project these build-owned facts; workers never
    /// resolve an implementation's dependencies through a global name index.
    pub fn native_callees(
        &self,
        function: &lash_kernel_value::names::FunctionId,
    ) -> Result<Vec<(lash_kernel_value::names::FunctionId, &'static str)>, String> {
        let image = lash_kernel_doc::ReleasedFunctions::read(self.image)
            .map_err(|error| error.to_string())?;
        let at = image
            .ids()
            .position(|id| id == *function)
            .ok_or_else(|| format!("{} holds no native {function}", self.path))?;
        Ok(image.listed(at).collect())
    }
}

macro_rules! package {
    ($path:literal, $stem:literal) => {
        BuiltPackage {
            path: $path,
            image: include_bytes!(concat!(env!("OUT_DIR"), "/", $stem, ".bin")),
            natives: include!(concat!(env!("OUT_DIR"), "/", $stem, ".natives.rs")),
        }
    };
}

/// lash's packages this build holds, each after the packages it requires:
/// `kernel` always, and those of the dialects its features name.
pub const PACKAGES: &[BuiltPackage] = &[
    package!("kernel", "kernel"),
    #[cfg(feature = "dialect-typescript")]
    package!("regex.ecma", "regex_ecma"),
    #[cfg(feature = "dialect-typescript")]
    package!("date.ecma", "date_ecma"),
    #[cfg(feature = "dialect-typescript")]
    package!("url.whatwg", "url_whatwg"),
    #[cfg(feature = "dialect-typescript")]
    package!("typescript", "typescript"),
    #[cfg(feature = "dialect-python")]
    package!("python", "python"),
];

/// The package at `path`, when this build holds it.
#[must_use]
pub fn package(path: &str) -> Option<&'static BuiltPackage> {
    PACKAGES.iter().find(|built| built.path == path)
}

/// Why lash's packages could not be assembled: a defect of the build,
/// never of a document.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("lash's packages do not assemble: {message}")]
pub struct LibraryError {
    message: String,
}

impl LibraryError {
    pub fn new(message: impl std::fmt::Display) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

/// This build's immutable helper image. Labels and source resolver roots are
/// metadata; they create no Documents, Links, package exports or authority.
#[derive(Clone, Debug)]
pub struct HelperRelease {
    pub helpers: Arc<BuildHelpers>,
    pub functions: FunctionRegistry,
    pub roots: std::collections::BTreeSet<FunctionId>,
    pub labels: BTreeMap<FunctionId, String>,
}
impl HelperRelease {
    pub fn resolver_roots(&self) -> std::collections::BTreeSet<FunctionId> {
        self.roots.clone()
    }
}

fn standard_in(
    slot: &OnceLock<Result<Arc<HelperRelease>, LibraryError>>,
    assemble: impl FnOnce() -> Result<Arc<HelperRelease>, LibraryError>,
) -> Result<Arc<HelperRelease>, LibraryError> {
    slot.get_or_init(assemble).clone()
}

/// Install this build's metadata once. Function images stay lazy; no helper
/// body is decoded, hashed or published while opening the release.
pub fn helper_release() -> Result<Arc<HelperRelease>, LibraryError> {
    static STANDARD: OnceLock<Result<Arc<HelperRelease>, LibraryError>> = OnceLock::new();
    standard_in(&STANDARD, || {
        let mut functions = FunctionRegistry::declarations();
        for built in PACKAGES {
            let image =
                lash_kernel_doc::ReleasedFunctions::read(built.image).map_err(LibraryError::new)?;
            functions
                .register_released(image)
                .map_err(LibraryError::new)?;
        }
        let roots = serde_json::from_str(include_str!(concat!(
            env!("OUT_DIR"),
            "/helpers.roots.json"
        )))
        .map_err(LibraryError::new)?;
        let members = serde_json::from_str(include_str!(concat!(
            env!("OUT_DIR"),
            "/helpers.members.json"
        )))
        .map_err(LibraryError::new)?;
        let helpers = Arc::new(
            BuildHelpers::from_frame(
                include!(concat!(env!("OUT_DIR"), "/helpers.identity.rs")),
                functions.clone(),
                members,
            )
            .map_err(LibraryError::new)?,
        );
        let labels: BTreeMap<FunctionId, serde_json::Value> = serde_json::from_str(include_str!(
            concat!(env!("OUT_DIR"), "/helpers.labels.json")
        ))
        .map_err(LibraryError::new)?;
        Ok(Arc::new(HelperRelease {
            helpers,
            functions,
            roots,
            labels: labels
                .into_iter()
                .map(|(id, label)| (id, label.to_string()))
                .collect(),
        }))
    })
}
