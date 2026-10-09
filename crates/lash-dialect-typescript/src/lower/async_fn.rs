//! Where asynchronous source is handed over.
//!
//! The lowerer calls these two functions for every `async` function and
//! every `await`, in whatever position they stand, and for nothing else.
//! Until asynchronous lowering is written, both refuse.

use super::{Lowerer, Lowering, Operand};
use crate::adapter as ast;
use crate::{Diagnostic, DiagnosticCode, SourceSpan};

impl Lowerer<'_> {
    /// An `async` function or arrow. Gives the operand of its value, a
    /// closure by the dialect's calling convention.
    pub(super) fn lower_async_function(
        &mut self,
        _function: &ast::Function,
        span: Option<SourceSpan>,
    ) -> Lowering<Operand> {
        Err(Diagnostic::new(
            DiagnosticCode::AsyncUnsupported,
            "async functions are not in the TypeScript dialect yet",
            span.or(self.span),
        ))
    }

    /// `await value`. The statements of everything the source evaluated
    /// before it are already emitted.
    pub(super) fn lower_await(
        &mut self,
        _value: &ast::Expr,
        span: SourceSpan,
    ) -> Lowering<Operand> {
        Err(Diagnostic::new(
            DiagnosticCode::AwaitUnsupported,
            "`await` is not in the TypeScript dialect yet",
            Some(span),
        ))
    }
}
