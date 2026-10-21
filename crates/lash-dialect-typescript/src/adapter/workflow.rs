//! Whole-workflow declarations read through the ordinary bounded adapter.
use lash_kernel_dialect::workflow::ModuleRefusal;

use super::*;

pub(crate) fn parse(source: &str, limits: SourceLimits) -> Result<Program, Diagnostic> {
    guard_source_size(source, limits)?;
    guard_source_nesting(source, limits.max_nesting())?;
    let mut program = parse_on_proportional_stack(source, true, limits)?;
    for statement in &mut program.statements {
        declaration(statement);
    }
    Ok(program)
}

/// Refuses a module that declares two entries of one name, at the second,
/// under the workflow contract's shared code.
pub(super) fn entries_once(program: &Program) -> Result<(), Diagnostic> {
    let mut seen = std::collections::BTreeSet::new();
    for statement in &program.statements {
        let (span, inner) = match statement {
            Stmt::Spanned(span, inner) => (Some(*span), inner.as_ref()),
            other => (None, other),
        };
        let names: Vec<&str> = match inner {
            Stmt::Function { name, function } => vec![name.as_str()],
            Stmt::Var {
                kind: VarKind::Const,
                declarations,
            } => declarations
                .iter()
                .filter_map(
                    |declaration| match (&declaration.pattern, &declaration.init) {
                        (Pattern::Ident(name, _), Some(Expr::Function(function))) => {
                            Some(name.as_str())
                        }
                        _ => None,
                    },
                )
                .collect(),
            _ => Vec::new(),
        };
        for name in names {
            if !seen.insert(name) {
                return Err(Diagnostic::module(ModuleRefusal::DuplicateEntry.at(
                    format!("the module declares more than one entry named `{name}`"),
                    span,
                )));
            }
        }
    }
    Ok(())
}

/// A top-level async declaration is an entry, using the same literal
/// lowering as a const-bound async function. Keep its labels and byte spans.
fn declaration(statement: &mut Stmt) {
    match statement {
        Stmt::Spanned(_, inner) => declaration(inner),
        Stmt::Function { name, function } => {
            *statement = Stmt::Var {
                kind: VarKind::Const,
                declarations: vec![Var {
                    pattern: Pattern::Ident(name.clone(), None),
                    init: Some(Expr::Function(function.clone())),
                }],
            };
        }
        _ => {}
    }
}
