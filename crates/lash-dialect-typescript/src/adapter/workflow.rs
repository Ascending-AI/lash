//! Whole-workflow declarations read through the ordinary bounded adapter.
use super::*;

pub(crate) fn parse(source: &str) -> Result<Program, Diagnostic> {
    guard_source_size(source)?;
    guard_source_nesting(source)?;
    let mut program = parse_on_proportional_stack(source, true)?;
    for statement in &mut program.statements {
        declaration(statement);
    }
    Ok(program)
}

/// A top-level async declaration is a named process, using the same literal
/// lowering as a const-bound async function. Keep its labels and byte spans.
fn declaration(statement: &mut Stmt) {
    match statement {
        Stmt::Spanned(_, inner) | Stmt::Labeled { stmt: inner, .. } => declaration(inner),
        Stmt::Function { name, function } if function.is_async => {
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
