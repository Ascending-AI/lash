//! Library re-linking is independent of the closed canvas vocabulary.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{
    ActKind, Action, Callee, Document, Expr, FunctionId, FunctionName, FunctionRegistry,
    InvalidReason, Site, StartAdapter, StartConvention, Stmt, Unit,
};

use crate::EditRefusal;
use crate::refusal::refused;
use crate::tree::NodeMut;

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
    let mut pending = vec![(
        Site::new(Unit::Main, []),
        NodeMut::Block(&mut document.main),
    )];
    for (name, function) in &mut document.functions {
        let site = Site::new(Unit::Function(name.clone()), []);
        if adapter(function.start.as_mut(), &mut replace)? {
            edited.insert(site.clone());
        }
        pending.push((site, NodeMut::Block(&mut function.body)));
    }
    while let Some((site, mut node)) = pending.pop() {
        let changed = match &mut node {
            NodeMut::Stmt(Stmt::For { via: Some(via), .. }) => {
                let mut changed = replace(&mut via.open)?;
                for function in [&mut via.next, &mut via.item, &mut via.check]
                    .into_iter()
                    .flatten()
                {
                    changed |= replace(function)?;
                }
                changed
            }
            NodeMut::Action(
                Action::Call {
                    callee: Callee::Library(function),
                    ..
                }
                | Action::Spawn {
                    callee: Callee::Library(function),
                    ..
                }
                | Action::Await {
                    policy: function, ..
                },
            )
            | NodeMut::Expr(Expr::Call { function, .. }) => replace(function)?,
            NodeMut::Expr(Expr::Act(act)) => match &mut act.kind {
                ActKind::Call(Callee::Library(function))
                | ActKind::Spawn(Callee::Library(function))
                | ActKind::Await(function) => replace(function)?,
                _ => false,
            },
            NodeMut::Expr(Expr::Closure(closure)) => adapter(closure.start.as_mut(), &mut replace)?,
            _ => false,
        };
        if changed {
            edited.insert(site.clone());
        }
        pending.extend(
            (0u32..)
                .zip(node.children())
                .map(|(index, child)| (site.child(index), child)),
        );
    }
    document.manifest.functions = lash_kernel_check::requirements(&document, registry).functions;
    lash_kernel_check::derive(&document, registry).map_err(|error| refused(None, error))?;
    Ok((document, edited))
}

fn adapter(
    start: Option<&mut StartAdapter>,
    replace: &mut impl FnMut(&mut FunctionId) -> Result<bool, RelinkRefusal>,
) -> Result<bool, RelinkRefusal> {
    if let Some(StartAdapter {
        convention: StartConvention::TsArgsList { await_fn, same_fn },
        ..
    }) = start
    {
        let changed = replace(await_fn)?;
        return Ok(replace(same_fn)? | changed);
    }
    Ok(false)
}
