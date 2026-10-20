//! Library re-linking is independent of the closed canvas vocabulary.
//!
//! A document is re-linked to one package set's view (K-LIB-009): each
//! library reference takes the identity the set defines under its name,
//! never a retained set's, so a node that still holds an earlier release
//! of a package re-links to the set it was asked for alone.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{Document, FunctionId, FunctionName, InvalidReason, Site};
use lash_kernel_package::PackageView;

use crate::EditRefusal;
use crate::refusal::refused;

/// A package set's view cannot unambiguously re-link a document, or the
/// result fails checking.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RelinkRefusal {
    #[error("the package set defines no library function named `{name}` (formerly {function})")]
    MissingFunction {
        function: FunctionId,
        name: FunctionName,
    },
    #[error("the package set defines more than one library function named `{name}`: {functions:?}")]
    AmbiguousFunction {
        name: FunctionName,
        functions: Vec<FunctionId>,
    },
    #[error("{0}")]
    Refused(#[from] EditRefusal),
}

pub(crate) fn relink(
    original: &Document,
    view: &PackageView<'_>,
) -> Result<(Document, BTreeSet<Site>), RelinkRefusal> {
    let mut named: BTreeMap<&str, Vec<FunctionId>> = BTreeMap::new();
    for (name, identity) in view.definitions() {
        named.entry(name).or_default().push(identity);
    }
    let registry = view.catalog().as_ref();
    let mut resolved = BTreeMap::new();
    let mut replace = |identity: &mut FunctionId| -> Result<bool, RelinkRefusal> {
        let replacement = if let Some(replacement) = resolved.get(identity) {
            *replacement
        } else {
            let name = original.manifest.functions.get(identity).ok_or_else(|| {
                RelinkRefusal::Refused(crate::EditRefusal {
                    diagnostics: vec![crate::EditDiagnostic {
                        edit: None,
                        location: None,
                        kind: crate::EditDiagnosticKind::Invalid(
                            InvalidReason::FunctionNotListed {
                                function: *identity,
                            },
                        ),
                    }],
                })
            })?;
            let replacement = match named
                .get(name.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                [] => {
                    return Err(RelinkRefusal::MissingFunction {
                        function: *identity,
                        name: name.clone(),
                    });
                }
                [replacement] => *replacement,
                functions => {
                    return Err(RelinkRefusal::AmbiguousFunction {
                        name: name.clone(),
                        functions: functions.to_vec(),
                    });
                }
            };
            resolved.insert(*identity, replacement);
            replacement
        };
        let changed = *identity != replacement;
        *identity = replacement;
        Ok(changed)
    };
    let mut document = original.clone();
    let mut edited = BTreeSet::new();
    document.try_each_function_mut(|site, function| {
        if replace(function)? {
            edited.insert(site.clone());
        }
        Ok::<(), RelinkRefusal>(())
    })?;
    document.manifest.functions = lash_kernel_check::requirements(&document, registry).functions;
    lash_kernel_check::derive(&document, registry).map_err(|error| refused(None, error))?;
    Ok((document, edited))
}
