//! A `package.stale_value` error as a host shows it: its two artifacts by
//! their release labels.

use lash_kernel_dialect::PackagePath;
use lash_kernel_doc::{ArtifactDigest, Datum, ErrorDatum, FunctionId};

use crate::HeldPackages;
use crate::shape::STALE_VALUE;

/// What a `package.stale_value` error names: the value type, the first
/// path that differs, the identity of the function that built the value,
/// when it holds one, and the checking function's own. Each identity
/// names its artifact through the artifacts a node holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaleValue {
    pub ty: String,
    pub at: String,
    pub built: Option<FunctionId>,
    pub current: FunctionId,
}

impl StaleValue {
    /// The stale value `error` reports, when it is one.
    pub fn of(error: &ErrorDatum) -> Option<Self> {
        if error.kind != STALE_VALUE {
            return None;
        }
        let Datum::Record(fields) = &error.data else {
            return None;
        };
        let field = |name: &str| {
            fields
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value)
        };
        let text = |name: &str| match field(name) {
            Some(Datum::Text(text)) => Some(text.clone()),
            _ => None,
        };
        let function = |name: &str| match field(name) {
            Some(Datum::Bytes(bytes)) => <[u8; 32]>::try_from(bytes.as_slice())
                .ok()
                .map(FunctionId::from_bytes),
            _ => None,
        };
        Some(Self {
            ty: text("type")?,
            at: text("at")?,
            built: function("built"),
            current: function("current")?,
        })
    }

    /// The message: the type, the path, and the release labels of the
    /// artifacts that hold the two functions, each with a short digest
    /// when the labels are equal or one is unknown. A function no held
    /// artifact of the type's package holds names an unknown release; one
    /// that several hold, unchanged across them, names every one of them,
    /// since it cannot tell which built the value.
    pub fn message(&self, held: &HeldPackages) -> String {
        let package = self
            .ty
            .rsplit_once('.')
            .map_or(self.ty.as_str(), |(package, _)| package);
        let path = PackagePath::parse(package).ok();
        let releases = |function: &FunctionId| {
            path.as_ref()
                .map_or_else(Vec::new, |path| releases(held, path, function))
        };
        let current = releases(&self.current);
        let built = self.built.as_ref().map_or_else(Vec::new, releases);
        let short =
            one(&built).is_none() || one(&current).is_none() || one(&built) == one(&current);
        let describe = |releases: &[Release<'_>], function: Option<&FunctionId>| match releases {
            [] => match function {
                Some(function) if short => {
                    let mut digest = function.to_string();
                    digest.truncate(12);
                    format!("an unknown release {digest}")
                }
                _ => "an unknown release".to_owned(),
            },
            [(label, _)] if !short => (*label).to_owned(),
            [(label, digest)] => format!("{label} {}", digest.short()),
            several => {
                let each: Vec<String> = several
                    .iter()
                    .map(|(label, digest)| format!("{label} {}", digest.short()))
                    .collect();
                format!("one of {}", each.join(", "))
            }
        };
        format!(
            "`{}` at `{}` was built by another release of `{package}` ({}; current {}); rebuild it",
            self.ty,
            self.at,
            describe(&built, self.built.as_ref()),
            describe(&current, Some(&self.current)),
        )
    }
}

/// A release of a package: its label and its artifact's digest.
type Release<'a> = (&'a str, ArtifactDigest);

/// The label of the one release of `releases`, when it is one.
fn one<'a>(releases: &[Release<'a>]) -> Option<&'a str> {
    match releases {
        [(label, _)] => Some(label),
        _ => None,
    }
}

/// Every release of the package at `path` whose code holds `function`, in
/// artifact key order: the one place the diagnosis finds which releases
/// hold a constructor or a check.
fn releases<'a>(
    held: &'a HeldPackages,
    path: &PackagePath,
    function: &FunctionId,
) -> Vec<Release<'a>> {
    held.artifacts()
        .iter()
        .filter(|(key, package)| key.path == *path && package.code().ids().contains(function))
        .map(|(key, package)| (package.release(), key.digest))
        .collect()
}
