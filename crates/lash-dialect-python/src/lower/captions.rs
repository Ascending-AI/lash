//! Which emitted statement owns an author construct's caption, decided as
//! the construct is lowered (spec §2): exactly one per construct, and never
//! scaffolding, a predeclaration or another construct's statement.

use lash_kernel_doc::{
    Caption, CaptionEntry, CaptionTarget, Expr, FieldPath, FieldRoot, FieldTarget, Node,
    ParamTarget, Stmt,
};

use lash_vocabulary::Name;
use lash_vocabulary::sites::{Site, Unit};
use ruff_python_ast::{self as ast, Stmt as PyStmt};
use ruff_text_size::{Ranged, TextSize};

use super::Lowerer;

/// The arm of the statement just emitted that a clause caption names.
#[derive(Clone, Copy)]
pub(super) enum Arm {
    /// A `try`'s catch block: its first `except`.
    Catch,
    /// The statement's last block: an `if`'s or a loop's `else`, or a
    /// `try`'s `finally`.
    Last,
}

impl Lowerer<'_> {
    /// The caption the source spells for the line that starts at `at`.
    pub(super) fn caption_at(&self, at: TextSize) -> Option<Caption> {
        self.captions.get(&at.to_usize()).cloned()
    }

    /// The caption of the clause header `keyword:` that is the only code
    /// between `from` and `to`, as a loop's `else` or a `try`'s `finally`
    /// is, whose position the syntax tree does not keep.
    pub(super) fn clause_caption(
        &self,
        from: TextSize,
        to: TextSize,
        keyword: &str,
    ) -> Option<Caption> {
        self.captions
            .range(from.to_usize()..to.to_usize())
            .find(|(at, _)| {
                self.source[**at..]
                    .strip_prefix(keyword)
                    .is_some_and(|rest| rest.trim_start_matches([' ', '\t']).starts_with(':'))
            })
            .map(|(_, caption)| caption.clone())
    }

    /// The caption of a loop's `else`, which follows `body`.
    pub(super) fn loop_else_caption(&self, body: &[PyStmt], orelse: &[PyStmt]) -> Option<Caption> {
        let (last, first) = (body.last()?, orelse.first()?);
        self.clause_caption(last.end(), first.start(), "else")
    }

    /// The caption of a `try`'s `finally`, which follows its last other
    /// suite.
    pub(super) fn finally_caption(&self, attempt: &ast::StmtTry) -> Option<Caption> {
        let first = attempt.finalbody.first()?;
        let before = attempt
            .orelse
            .last()
            .map(Ranged::end)
            .or_else(|| attempt.handlers.last().map(Ranged::end))
            .or_else(|| attempt.body.last().map(Ranged::end))?;
        self.clause_caption(before, first.start(), "finally")
    }

    /// The caption of a `try`'s first `except`; a later one's is a comment.
    pub(super) fn except_caption(&self, attempt: &ast::StmtTry) -> Option<Caption> {
        self.caption_at(attempt.handlers.first()?.start())
    }

    /// The captions `#:` lines spell above the parameters of the entry
    /// `def`, each written on its own line, and above the fields of the
    /// `TypedDict`s its parameters and result are annotated with. `names`
    /// are its parameters, in order.
    pub(super) fn signature_captions(
        &self,
        def: &ast::StmtFunctionDef,
        names: &[Name],
    ) -> Vec<CaptionEntry> {
        let function = Name::new(def.name.id.as_str());
        let above = |at: TextSize| self.caption_at(at);
        let field = |root: FieldRoot, (fields, caption): (Vec<String>, Caption)| CaptionEntry {
            target: CaptionTarget::Field(FieldTarget {
                function: function.clone(),
                path: FieldPath { root, fields },
            }),
            caption,
        };
        let mut entries = Vec::new();
        for (param, name) in def.parameters.args.iter().zip(names) {
            if let Some(caption) = above(param.start()) {
                entries.push(CaptionEntry {
                    target: CaptionTarget::Param(ParamTarget {
                        function: function.clone(),
                        param: name.clone(),
                    }),
                    caption,
                });
            }
            if let Some(annotation) = param.parameter.annotation.as_deref() {
                entries.extend(
                    super::typed_dicts::field_captions(annotation, &self.typed_dicts, &above)
                        .into_iter()
                        .map(|found| field(FieldRoot::Param(name.clone()), found)),
                );
            }
        }
        if let Some(returns) = def.returns.as_deref() {
            entries.extend(
                super::typed_dicts::field_captions(returns, &self.typed_dicts, &above)
                    .into_iter()
                    .map(|found| field(FieldRoot::Result, found)),
            );
        }
        entries.sort_by(|a, b| a.target.cmp(&b.target));
        entries.dedup_by(|later, first| later.target == first.target);
        entries
    }

    /// Gives `caption` to `arm` of the statement just emitted.
    pub(super) fn caption_arm(&mut self, arm: Arm, caption: Option<Caption>) {
        let (Some(caption), Some(note)) = (caption, self.buf.notes.last_mut()) else {
            return;
        };
        let index = match arm {
            Arm::Catch => 1,
            Arm::Last => note.blocks.len().saturating_sub(1),
        };
        note.arm_captions.insert(index, caption);
    }

    /// Gives the caption above `statement` to the one statement that owns
    /// it among those its lowering emitted from `first` on: a control's
    /// own statement, a `def`'s binding, and otherwise the statement that
    /// holds its first action, else its last. An entry's caption is its
    /// function's. A construct that emits nothing, such as a docstring or
    /// `pass`, leaves its caption a comment.
    pub(super) fn own_caption(&mut self, statement: &PyStmt, first: usize) {
        let Some(caption) = self.caption_at(statement.start()) else {
            return;
        };
        let emitted = first..self.buf.stmts.len();
        let last = |matches: fn(&Stmt) -> bool| {
            emitted
                .clone()
                .rev()
                .find(|&index| matches(&self.buf.stmts[index]))
        };
        let owner = match statement {
            PyStmt::FunctionDef(def) if self.entry_defs.contains(&def.range) => {
                self.declared_captions.push(CaptionEntry::site(
                    Site::new(Unit::Function(Name::new(def.name.id.as_str())), Vec::new()),
                    caption,
                ));
                return;
            }
            PyStmt::If(_) => last(|stmt| matches!(stmt, Stmt::If { .. })),
            PyStmt::For(_) | PyStmt::While(_) => {
                last(|stmt| matches!(stmt, Stmt::For { .. } | Stmt::While { .. }))
            }
            PyStmt::Try(_) => last(|stmt| matches!(stmt, Stmt::Try(_))),
            PyStmt::FunctionDef(_) => emitted.clone().last(),
            _ => emitted
                .clone()
                .find(|&index| acts(Node::Stmt(&self.buf.stmts[index])))
                .or_else(|| emitted.clone().last()),
        };
        if let Some(owner) = owner {
            self.buf.notes[owner].caption = Some(caption);
        }
    }
}

/// Whether `node` holds an action outside its blocks, which are statements
/// of their own.
fn acts(node: Node<'_>) -> bool {
    node.children().into_iter().any(|child| match child {
        Node::Block(_) => false,
        Node::Action(_) | Node::Expr(Expr::Act(_)) => true,
        other => acts(other),
    })
}
