//! The `defineProcess` wrapper: the one place that builds it and the one place
//! that reads it back.
//!
//! `defineProcess({ run })` has no lashlang counterpart. It lowers to a process
//! declaration whose body is a wrapper — `Try(Finish(Call(Function)))` with a
//! generated catch that turns an uncaught error into process failure — around
//! the authored `run` body. Only that inner body was written by a user, so the
//! lens projects and prints it rather than the wrapper, and the AST path to it
//! is the prefix every node id and execution site inside the process is keyed
//! on.
//!
//! Builder and reader live together because they are one fact. The wrapper's
//! shape was previously rebuilt from a description of it inside the lens, two
//! crates' modules apart from the code that emits it, which is a correlation
//! bug waiting for the next wrapper change (FIG-3057).

use lashlang::{CatchClause, Expr, ProcessDecl, TryExpr};

use super::GENERATED_BINDING_PREFIX;

/// The suffix the wrapper's generated catch binding carries.
const PROCESS_ERROR_SUFFIX: &str = "process_error";

/// The generated binding the wrapper catches process failure into.
fn process_error_binding() -> String {
    format!("{GENERATED_BINDING_PREFIX}{PROCESS_ERROR_SUFFIX}")
}

/// Wraps an authored `run` closure as a process body.
///
/// `run(...args)` finishes the process with the closure's value, and anything
/// it throws fails the process instead of escaping as a runtime error.
pub(crate) fn process_run_wrapper(closure: Expr, call_args: Vec<Expr>) -> Expr {
    let failure_name = process_error_binding();
    Expr::Try(Box::new(TryExpr {
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
    }))
}

/// The authored `run` body of a lowered `defineProcess`, with its AST path.
///
/// The returned path is the prefix that addresses that body inside the process
/// declaration. It is read off the AST — each step is the position of the
/// chosen child in [`Expr::children`], the same spelling `execution_sites` and
/// the compiler use — rather than transcribed as a constant, so a change to
/// [`process_run_wrapper`] moves the path with it instead of silently
/// decorrelating node identity from the runtime's sites.
///
/// `None` for any process body this module did not build: a lashlang-authored
/// process has no wrapper and no inner body to unwrap.
pub(crate) fn process_run_body_path(process: &ProcessDecl) -> Option<(Vec<u32>, &Expr)> {
    let (path, body) = _wrapped_run_body_path(&process.body)?;
    Some((path, body))
}

/// The authored body inside a wrapper-shaped process body, ignoring the path.
pub(crate) fn wrapped_run_body(body: &Expr) -> Option<&Expr> {
    _wrapped_run_body_path(body).map(|(_, body)| body)
}

fn _wrapped_run_body_path(wrapper: &Expr) -> Option<(Vec<u32>, &Expr)> {
    let mut path = Vec::new();

    let Expr::Try(try_expr) = wrapper else {
        return None;
    };
    let TryExpr {
        body,
        catch: Some(CatchClause {
            binding,
            body: catch_body,
        }),
        finally: None,
    } = try_expr.as_ref()
    else {
        return None;
    };
    if !binding.starts_with(GENERATED_BINDING_PREFIX) || !binding.ends_with(PROCESS_ERROR_SUFFIX) {
        return None;
    }
    match catch_body.as_ref() {
        Expr::Fail(value) => match value.as_ref() {
            Expr::Variable(name) if name == binding => {}
            _ => return None,
        },
        _ => return None,
    }

    let finish = body.as_ref();
    step(wrapper, finish, &mut path)?;
    let Expr::Finish(call) = finish else {
        return None;
    };
    let call = call.as_ref();
    step(finish, call, &mut path)?;
    let Expr::Call { function, .. } = call else {
        return None;
    };
    let function = function.as_ref();
    step(call, function, &mut path)?;
    let Expr::Function(run) = function else {
        return None;
    };
    let run_body = &run.body;
    step(function, run_body, &mut path)?;
    Some((path, run_body))
}

/// Appends the position of `child` among `parent`'s children to `path`.
fn step(parent: &Expr, child: &Expr, path: &mut Vec<u32>) -> Option<()> {
    let index = child_index(parent, child)?;
    path.push(index);
    Some(())
}

fn child_index(parent: &Expr, child: &Expr) -> Option<u32> {
    let index = parent.children().position(|candidate| {
        std::ptr::eq(std::ptr::from_ref(candidate), std::ptr::from_ref(child))
    })?;
    u32::try_from(index).ok()
}

/// The authored body and its wrapper AST path of any wrapper-shaped process
/// body, not just a declaration's.
pub(crate) fn process_run_body_path_of(body: &Expr) -> Option<(Vec<u32>, &Expr)> {
    _wrapped_run_body_path(body)
}
