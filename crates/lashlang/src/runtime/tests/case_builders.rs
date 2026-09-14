//! Shared program shapes for the runtime case modules.
//!
//! ADR 0096 retired the authored Lashlang surface, so these suites build their
//! inputs from the AST. The handful of shapes that more than one case module
//! needs live here; single-use shapes stay next to their tests.

use super::*;

/// `tools.echo({ value: <value> })`
pub(super) fn echo(value: Expr) -> Expr {
    builders::receiver_call(
        builders::resource(&["tools"]),
        "echo",
        vec![builders::record(vec![("value", value)])],
    )
}

/// `tools.echo({ value: <value> })?`
pub(super) fn echo_unwrap(value: Expr) -> Expr {
    builders::unwrap(echo(value))
}

/// `await tools.echo({ value: <value> })?`
pub(super) fn await_echo_unwrap(value: Expr) -> Expr {
    builders::await_expr(echo_unwrap(value))
}

/// `tools.err({})`
pub(super) fn err_call() -> Expr {
    builders::receiver_call(
        builders::resource(&["tools"]),
        "err",
        vec![builders::record(Vec::new())],
    )
}

/// `finish <expr>`
pub(super) fn finish_program(expr: Expr) -> Program {
    builders::program(vec![builders::finish(expr)])
}

/// `finish <left> <op> <right>`
pub(super) fn finish_binary(left: Expr, op: BinaryOp, right: Expr) -> Program {
    finish_program(builders::binary(left, op, right))
}
