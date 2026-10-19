//! The captions a workflow module's statements and clause arms carry, read
//! from the comments before them as the adapter converts each statement.

use swc_common::{BytePos, Spanned};
use swc_ecma_ast as swc;

use super::{Adapter, Stmt};
use crate::Diagnostic;
use crate::captions::Arm;

impl Adapter<'_> {
    /// A statement of a statement list, which the caption before it names.
    /// `export` is where the `export` keyword before a declaration starts:
    /// a caption there names the declaration, when none stands nearer.
    pub(super) fn convert_listed(
        &self,
        stmt: &swc::Stmt,
        export: Option<BytePos>,
    ) -> Result<Stmt, Diagnostic> {
        let converted = self.convert_stmt(stmt)?;
        let start = stmt.span().lo;
        if let Some(caption) = [Some(start), export]
            .into_iter()
            .flatten()
            .find_map(|at| self.comments.before(at.0 as usize))
        {
            self.captions
                .borrow_mut()
                .statements
                .insert(start.0 as usize, caption.clone());
        }
        Ok(converted)
    }

    /// Records the caption before the keyword that follows code ending at
    /// `end` as `arm`'s, of the statement starting at `start`.
    fn caption_arm(&self, start: BytePos, end: BytePos, arm: Arm) {
        if let Some(caption) = self.comments.before_next(end.0 as usize) {
            self.captions
                .borrow_mut()
                .arms
                .insert((start.0 as usize, arm), caption.clone());
        }
    }

    /// The caption before an `if`'s `else`: its else arm's, or, before
    /// `else if`, the inner `if`'s, as one nearer that `if` is.
    pub(super) fn caption_else(&self, stmt: &swc::IfStmt) {
        let Some(alternate) = stmt.alt.as_deref() else {
            return;
        };
        if !matches!(alternate, swc::Stmt::If(_)) {
            self.caption_arm(stmt.span.lo, stmt.cons.span().hi, Arm::Else);
            return;
        }
        let start = alternate.span().lo;
        if let Some(caption) = self
            .comments
            .before(start.0 as usize)
            .or_else(|| self.comments.before_next(stmt.cons.span().hi.0 as usize))
        {
            self.captions
                .borrow_mut()
                .statements
                .insert(start.0 as usize, caption.clone());
        }
    }

    /// The captions before a `try`'s `catch` and `finally`.
    pub(super) fn caption_clauses(&self, stmt: &swc::TryStmt) {
        let mut end = stmt.block.span.hi;
        if let Some(handler) = &stmt.handler {
            self.caption_arm(stmt.span.lo, end, Arm::Catch);
            end = handler.span.hi;
        }
        if stmt.finalizer.is_some() {
            self.caption_arm(stmt.span.lo, end, Arm::Finally);
        }
    }
}
