//! Building blocks for programs that have no TypeScript spelling.
//!
//! ADR 0096 makes TypeScript the sole authored RLM dialect; lashlang names the
//! IR and the VM. A few of the facts these suites pin outlive the surface that
//! used to spell them — the `format` builtin's placeholder rules, the numeric
//! `range`/`ceil_div`/`floor_div` helpers, tuple values — because they are
//! properties of the IR rather than of any dialect. Those tests build the IR
//! directly instead of authoring source, so they keep pinning the VM without
//! naming a retired syntax.

use lashlang::{Expr, Program};

/// A program whose body is `expressions`, run in order.
pub fn program(expressions: Vec<Expr>) -> Program {
    Program::block(expressions)
}

/// A single-expression program that finishes with `expr`.
pub fn finish_program(expr: Expr) -> Program {
    program(vec![finish(expr)])
}

pub fn finish(expr: Expr) -> Expr {
    Expr::Finish(Box::new(expr))
}

pub fn call(name: &str, args: Vec<Expr>) -> Expr {
    Expr::BuiltinCall {
        name: name.into(),
        args,
    }
}

pub fn string(value: &str) -> Expr {
    Expr::String(value.into())
}

pub fn number(value: f64) -> Expr {
    Expr::Number(value)
}

pub fn list(items: Vec<Expr>) -> Expr {
    Expr::List(items)
}
