//! A `package.stale_value` error as a host shows it: its two artifacts by
//! their release labels.

use lash_kernel_doc::ArtifactDigest;
use lash_kernel_doc::Datum;
use lash_kernel_doc::ErrorDatum;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::names::PackagePath;

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
        self.message_with(held, &[])
    }

    /// [`Self::message`], naming releases among the node's own artifacts
    /// and the packages of `surfaces`, the stored sets the reader read
    /// (FIG-6993).
    pub fn message_with(
        &self,
        held: &HeldPackages,
        surfaces: &[&crate::SurfaceSnapshot],
    ) -> String {
        let known = held
            .artifacts()
            .values()
            .map(|package| Known::of(package))
            .chain(surfaces.iter().flat_map(|surface| {
                surface.packages().map(|package| Known {
                    key: package.key(),
                    release: package.release(),
                    holds: Box::new(|function: &FunctionId| {
                        surface
                            .package_definitions(package.path())
                            .is_some_and(|definitions| definitions.contains(function))
                    }),
                })
            }))
            .collect();
        self.describe(known)
    }

    /// [`Self::message`], naming releases among `packages`: the artifacts
    /// of every release a reader knows, the node's own set's and any other
    /// build's it read.
    pub fn message_among(&self, packages: &[&crate::Package]) -> String {
        self.describe(packages.iter().map(|package| Known::of(package)).collect())
    }

    fn describe(&self, packages: Vec<Known<'_>>) -> String {
        let package = self
            .ty
            .rsplit_once('.')
            .map_or(self.ty.as_str(), |(package, _)| package);
        let path = PackagePath::parse(package).ok();
        let releases = |function: &FunctionId| {
            path.as_ref()
                .map_or_else(Vec::new, |path| releases(&packages, path, function))
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

/// A release a reader knows: its artifact's key, its label, and whether
/// its code holds a function.
struct Known<'a> {
    key: crate::ArtifactKey,
    release: &'a str,
    holds: Box<dyn Fn(&FunctionId) -> bool + 'a>,
}

impl<'a> Known<'a> {
    fn of(package: &'a crate::Package) -> Self {
        Self {
            key: package.key(),
            release: package.release(),
            holds: Box::new(|function: &FunctionId| package.code().ids().contains(function)),
        }
    }
}

/// Every release of the package at `path` whose code holds `function`, in
/// artifact key order: the one place the diagnosis finds which releases
/// hold a constructor or a check.
fn releases<'a>(
    packages: &[Known<'a>],
    path: &PackagePath,
    function: &FunctionId,
) -> Vec<Release<'a>> {
    let mut found: Vec<(&crate::ArtifactKey, &'a str)> = packages
        .iter()
        .filter(|known| known.key.path == *path && (known.holds)(function))
        .map(|known| (&known.key, known.release))
        .collect();
    found.sort_by_key(|(key, _)| *key);
    found.dedup_by(|(a, _), (b, _)| a == b);
    found
        .into_iter()
        .map(|(key, release)| (release, key.digest))
        .collect()
}
