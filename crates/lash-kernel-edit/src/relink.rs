//! Library re-linking is independent of the closed canvas vocabulary.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{Document, FunctionId, FunctionName, FunctionRegistry, InvalidReason, Site};

use crate::EditRefusal;
use crate::refusal::refused;

/// A registry cannot unambiguously re-link a document, or the result fails checking.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RelinkRefusal {
    #[error("no registered library function is named `{name}` (formerly {function})")]
    MissingFunction {
        function: FunctionId,
        name: FunctionName,
    },
    #[error("more than one registered library function is named `{name}`: {functions:?}")]
    AmbiguousFunction {
        name: FunctionName,
        functions: Vec<FunctionId>,
    },
    #[error("{0}")]
    Refused(#[from] EditRefusal),
}

pub(crate) fn relink(
    original: &Document,
    registry: &FunctionRegistry,
) -> Result<(Document, BTreeSet<Site>), RelinkRefusal> {
    let mut named: BTreeMap<&FunctionName, Vec<FunctionId>> = BTreeMap::new();
    for (identity, registered) in registry.iter() {
        named
            .entry(&registered.definition().name)
            .or_default()
            .push(*identity);
    }
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
            let replacement = match named.get(name).map(Vec::as_slice).unwrap_or_default() {
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
