//! The process fixtures the end-state tests link and publish.
//!
//! ADR 0096 retired the Lashlang front-end, so each witness states its AST; the
//! source it stands for is the doc comment above it.

use lashlang::testing::ast_builders as b;

/// `process main() signals { ready: <ty> } { value = wait_signal("ready")
/// finish <finish> }`
///
/// ADR 0096 retired the Lashlang front-end, so these process fixtures state
/// their AST; the source each one stands for is kept at the call site.
pub(super) fn wait_signal_process(
    signal_ty: lashlang::TypeExpr,
    finish: lashlang::Expr,
) -> lashlang::Program {
    b::module(
        vec![b::process_with_signals(
            "main",
            Vec::new(),
            vec![b::signal("ready", signal_ty)],
            b::block(vec![
                b::assign("value", b::wait_signal("ready")),
                b::finish(finish),
            ]),
        )],
        Vec::new(),
    )
}

/// A `child` process finishing `{ from: "child" }` alongside a `main` that
/// starts it, awaits the handle and finishes `<finish>` over the joined value.
pub(super) fn child_join_process(finish: lashlang::Expr) -> lashlang::Program {
    b::module(
        vec![
            b::process(
                "child",
                Vec::new(),
                b::finish(b::record(vec![("from", b::string("child"))])),
            ),
            b::process(
                "main",
                Vec::new(),
                b::block(vec![
                    b::assign("handle", b::start("child", Vec::new())),
                    b::assign("value", b::await_expr(b::var("handle"))),
                    b::finish(finish),
                ]),
            ),
        ],
        Vec::new(),
    )
}
