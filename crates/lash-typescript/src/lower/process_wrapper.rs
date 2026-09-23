//! The process-body wrapper: the one place that builds it.
//!
//! An authored process literal — a top-level `async` arrow — has no lashlang
//! counterpart. It lowers to a process body wrapped in the language-neutral
//! [`StructuralRole::ProcessWrapper`]: `Try(Finish(Call(run)))` with a catch
//! that turns an uncaught error into process failure, around the authored
//! arrow body. Every reader finds the authored body through the role
//! ([`lashlang::process_wrapper_run_path`]), never through the catch binding's
//! spelling.

use lashlang::{CatchClause, Expr, StructuralRole, TryExpr};

use super::GENERATED_BINDING_PREFIX;

/// The generated binding the wrapper catches process failure into.
fn process_error_binding() -> String {
    format!("{GENERATED_BINDING_PREFIX}process_error")
}

/// `run(...args)` finishes the process with the closure's value, and anything
/// it throws fails the process instead of escaping as a runtime error.
pub(crate) fn process_run_wrapper(closure: Expr, call_args: Vec<Expr>) -> Expr {
    let failure_name = process_error_binding();
    Expr::Role {
        role: StructuralRole::ProcessWrapper,
        expr: Box::new(Expr::Try(Box::new(TryExpr {
            body: Box::new(Expr::Finish(Box::new(Expr::Call {
                function: Box::new(closure),
                args: call_args,
            }))),
            catch: Some(CatchClause {
                binding: failure_name.as_str().into(),
                body: Box::new(Expr::Fail(Box::new(Expr::Variable(
                    failure_name.as_str().into(),
                )))),
            }),
            finally: None,
        }))),
    }
}

/// The authored run body inside a process-wrapper body, or `None` for a body
/// that is not wrapped, such as a directly authored process.
pub(crate) fn wrapped_run_body(body: &Expr) -> Option<&Expr> {
    let Expr::Role {
        role: StructuralRole::ProcessWrapper,
        expr,
    } = body
    else {
        return None;
    };
    lashlang::process_wrapper_run_path(expr).map(|(_, body)| body)
}
