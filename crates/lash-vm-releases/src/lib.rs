//! Build-generated helper Documents, ordinary package namespaces and native
//! declarations. Runtime references retain their Link DAG; there is no set.

use std::sync::{Arc, OnceLock};

use lash_kernel_doc::{Document, FunctionRegistry};

use lash_kernel_package::LinkedContent;
use lash_vocabulary::{names::*, package::*};
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
    pub natives: &'static [(lash_vocabulary::names::FunctionId, &'static str)],
}

impl BuiltPackage {
    /// The identities and local names declared by this function's body.
    /// Constructor tables project these build-owned facts; workers never
    /// resolve an implementation's dependencies through a global name index.
    pub fn native_callees(
        &self,
        function: &lash_vocabulary::names::FunctionId,
    ) -> Result<Vec<(lash_vocabulary::names::FunctionId, &'static str)>, String> {
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

/// The helper release is executable declarations plus its linked namespace.
/// Native bodies are supplied separately by the worker embedding.
#[derive(Clone, Debug)]
pub struct HelperRelease {
    pub content: LinkedContent,
    pub functions: FunctionRegistry,
    /// Display sidecars keyed by exact Package identity, outside executable content.
    pub labels: BTreeMap<PackageId, String>,
}

impl HelperRelease {
    /// Resolve only authored exports; lifted local functions remain reachable
    /// by identity but never enter the global source-name resolver.
    pub fn resolver_roots(&self) -> std::collections::BTreeSet<FunctionId> {
        self.content
            .packages
            .values()
            .flat_map(|package| package.members.values())
            .filter_map(|member| {
                let MemberKind::Export { link, export } = &member.kind else {
                    return None;
                };
                let link = self.content.links.get(link)?;
                let document = self.content.documents.get(&link.document)?;
                match document.exports.get(&export.name)? {
                    lash_kernel_doc::Export::Function(function) => Some(function.function),
                    _ => None,
                }
            })
            .collect()
    }
}

fn standard_in(
    slot: &OnceLock<Result<Arc<HelperRelease>, LibraryError>>,
    assemble: impl FnOnce() -> Result<Arc<HelperRelease>, LibraryError>,
) -> Result<Arc<HelperRelease>, LibraryError> {
    slot.get_or_init(assemble).clone()
}

/// Assemble the build's one helper Document, and retain registry declarations
/// independently of package provenance. This does not install durable roots.
pub fn helper_release() -> Result<Arc<HelperRelease>, LibraryError> {
    static STANDARD: OnceLock<Result<Arc<HelperRelease>, LibraryError>> = OnceLock::new();
    standard_in(&STANDARD, || {
        let document: Document = serde_json::from_str(include_str!(concat!(
            env!("OUT_DIR"),
            "/helpers.document.json"
        )))
        .map_err(LibraryError::new)?;
        let mut functions = FunctionRegistry::declarations();
        for built in PACKAGES {
            let image =
                lash_kernel_doc::ReleasedFunctions::read(built.image).map_err(LibraryError::new)?;
            functions
                .register_released(image)
                .map_err(LibraryError::new)?;
        }
        let mut content = LinkedContent::default();
        let document_id = document.identity();
        let fills = document
            .imports
            .iter()
            .map(|(slot, import)| {
                lash_kernel_dialect::authoring::service_requirement(&import.name)
                    .map(|requirement| (*slot, Fill::Operation(requirement)))
                    .ok_or_else(|| {
                        LibraryError::new(format!(
                            "helper Import {:?} has no build-defined operation offer",
                            import.name
                        ))
                    })
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let link = Link {
            document: document_id,
            fills,
        };
        let link_id = link.identity();
        let packages: BTreeMap<PackagePath, BTreeMap<Segment, ExportName>> = serde_json::from_str(
            include_str!(concat!(env!("OUT_DIR"), "/helpers.packages.json")),
        )
        .map_err(LibraryError::new)?;
        for (path, exports) in packages {
            let members = exports
                .into_iter()
                .map(|(name, export)| {
                    (
                        name,
                        Member {
                            visibility: Visibility::Public,
                            kind: MemberKind::Export {
                                link: link_id,
                                export,
                            },
                        },
                    )
                })
                .collect();
            let package = Package { path, members };
            content.packages.insert(package.identity(), package);
        }
        content.documents.insert(document_id, document);
        content.links.insert(link_id, link);
        let authored: BTreeMap<PackagePath, serde_json::Value> = serde_json::from_str(
            include_str!(concat!(env!("OUT_DIR"), "/helpers.labels.json")),
        )
        .map_err(LibraryError::new)?;
        let labels = content
            .packages
            .iter()
            .filter_map(|(id, package)| {
                authored
                    .get(&package.path)
                    .map(|labels| (*id, labels.to_string()))
            })
            .collect();
        Ok(Arc::new(HelperRelease {
            content,
            functions,
            labels,
        }))
    })
}
