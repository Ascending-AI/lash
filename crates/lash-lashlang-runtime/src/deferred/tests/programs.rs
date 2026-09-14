//! The fixture programs the deferred-resolution tests link.
//!
//! ADR 0096 retired the Lashlang front-end, so each witness states its AST; the
//! source it stands for is the doc comment above it.

use lashlang::testing::ast_builders as b;

/// `await web.fetch({ url: "x" })?`
pub(super) fn web_fetch_url_program() -> lashlang::Program {
    b::program(vec![b::module_call(
        &["web"],
        "fetch",
        vec![b::record(vec![("url", b::string("x"))])],
    )])
}

/// `await web.fetch({})?`
pub(super) fn web_fetch_program() -> lashlang::Program {
    b::program(vec![web_fetch()])
}

/// `await mystery.run({})?`
pub(super) fn mystery_run_program() -> lashlang::Program {
    b::program(vec![mystery_run()])
}

/// `await web.fetch({})?` / `await mystery.run({})?` / `await web.fetch({})?`
pub(super) fn web_mystery_web_program() -> lashlang::Program {
    b::program(vec![web_fetch(), mystery_run(), web_fetch()])
}

/// `await web.fetch({})?` / `await mystery.run({})?`
pub(super) fn web_then_mystery_program() -> lashlang::Program {
    b::program(vec![web_fetch(), mystery_run()])
}

fn web_fetch() -> lashlang::Expr {
    b::module_call(&["web"], "fetch", vec![b::record(Vec::new())])
}

fn mystery_run() -> lashlang::Expr {
    b::module_call(&["mystery"], "run", vec![b::record(Vec::new())])
}
