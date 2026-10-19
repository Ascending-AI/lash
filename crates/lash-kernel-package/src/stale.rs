//! A `package.stale_value` error as a host shows it: its two artifacts by
//! their release labels.

use lash_kernel_dialect::PackagePath;
use lash_kernel_doc::{ArtifactDigest, Datum, ErrorDatum};

use crate::shape::STALE_VALUE;
use crate::{ArtifactKey, HeldPackages};

/// What a `package.stale_value` error names: the value type, the first
/// path that differs, the artifact that built the value, when it holds
/// one, and the checking member's own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaleValue {
    pub ty: String,
    pub at: String,
    pub built: Option<ArtifactDigest>,
    pub current: ArtifactDigest,
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
        let digest = |name: &str| match field(name) {
            Some(Datum::Bytes(bytes)) => <[u8; 32]>::try_from(bytes.as_slice())
                .ok()
                .map(ArtifactDigest::from_bytes),
            _ => None,
        };
        Some(Self {
            ty: text("type")?,
            at: text("at")?,
            built: digest("built"),
            current: digest("current")?,
        })
    }

    /// The message: the type, the path, and the release labels of both
    /// artifacts, each with a short digest when the labels are equal or
    /// one is unknown.
    pub fn message(&self, held: &HeldPackages) -> String {
        let package = self
            .ty
            .rsplit_once('.')
            .map_or(self.ty.as_str(), |(package, _)| package);
        let label = |digest: &ArtifactDigest| {
            PackagePath::parse(package).ok().and_then(|path| {
                held.artifacts()
                    .get(&ArtifactKey {
                        path,
                        digest: *digest,
                    })
                    .map(|artifact| artifact.release().to_owned())
            })
        };
        let current = label(&self.current);
        let built = self.built.as_ref().and_then(label);
        let short = built.is_none() || current.is_none() || built == current;
        let describe = |label: Option<String>, digest: Option<&ArtifactDigest>| {
            let label = label.unwrap_or_else(|| "an unknown release".to_owned());
            match digest {
                Some(digest) if short => format!("{label} {}", digest.short()),
                _ => label,
            }
        };
        format!(
            "`{}` at `{}` was built by another release of `{package}` ({}; current {}); rebuild it",
            self.ty,
            self.at,
            describe(built, self.built.as_ref()),
            describe(current, Some(&self.current)),
        )
    }
}
