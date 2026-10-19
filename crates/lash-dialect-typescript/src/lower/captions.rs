//! Which emitted statement owns each caption the source spells: exactly one
//! per author construct, chosen as its lowering ends, so scaffolding,
//! hoisted temporaries and predeclarations never own one by accident.

use lash_kernel_doc::{Expr, Name, Node, Stmt};

use super::Lowerer;
use crate::adapter::{self as ast, VarKind};
use crate::captions::Arm;

/// Whether a kernel statement is the one a control construct lowers to:
/// the last of its kind the construct emits.
type Control = fn(&Stmt) -> bool;

fn control(statement: &ast::Stmt) -> Option<Control> {
    Some(match statement {
        ast::Stmt::If { .. } => |stmt| matches!(stmt, Stmt::If { .. }),
        ast::Stmt::While { .. }
        | ast::Stmt::DoWhile { .. }
        | ast::Stmt::For { .. }
        | ast::Stmt::Switch { .. } => |stmt| matches!(stmt, Stmt::While { .. }),
        ast::Stmt::ForOf { .. } | ast::Stmt::ForIn { .. } => {
            |stmt| matches!(stmt, Stmt::For { .. })
        }
        ast::Stmt::Try { .. } => |stmt| matches!(stmt, Stmt::Try(_)),
        _ => return None,
    })
}

/// Whether `node` performs an action itself, outside its blocks.
fn acts(node: Node<'_>) -> bool {
    node.children().into_iter().any(|child| match child {
        Node::Block(_) => false,
        Node::Action(_) | Node::Expr(Expr::Act(_)) => true,
        other => acts(other),
    })
}

/// The index, among a statement's blocks, of the arm `arm` names.
fn arm_index(statement: &Stmt, arm: Arm) -> Option<usize> {
    match (statement, arm) {
        (Stmt::If { .. }, Arm::Else) => Some(1),
        (Stmt::Try(scope), Arm::Catch) if scope.catch.is_some() => Some(1),
        (Stmt::Try(scope), Arm::Finally) if scope.finally.is_some() => {
            Some(1 + usize::from(scope.catch.is_some()))
        }
        _ => None,
    }
}

impl Lowerer<'_> {
    /// The entry a module's top-level `const` declares, which a caption
    /// before it names.
    pub(super) fn declared_entry(&self, statement: &ast::Stmt) -> Option<Name> {
        let ast::Stmt::Var {
            kind: VarKind::Const,
            declarations,
        } = statement.unspanned()
        else {
            return None;
        };
        declarations.iter().find_map(|declaration| {
            match (&declaration.pattern, &declaration.init) {
                (ast::Pattern::Ident(name, _), Some(init)) if self.module_entry(init).is_some() => {
                    Some(Name::new(name.as_str()))
                }
                _ => None,
            }
        })
    }

    /// Hands the captions spelled for the statement starting at `start`,
    /// and for its arms, to their owners among what lowering it emitted
    /// from `before` on: `entry`'s function, when it declares one; a
    /// control's own statement; else the statement holding the construct's
    /// first action, or failing that the last it emitted. A statement that
    /// owns a nearer construct's caption keeps it.
    pub(super) fn own_captions(
        &mut self,
        start: usize,
        statement: &ast::Stmt,
        before: usize,
        entry: Option<Name>,
    ) {
        let spelled = self.spelled;
        let caption = spelled.statements.get(&start);
        if let Some(entry) = entry {
            if let Some(caption) = caption {
                self.declared_captions
                    .push(lash_kernel_doc::CaptionEntry::site(
                        lash_kernel_doc::Site::new(
                            lash_kernel_doc::Unit::Function(entry),
                            Vec::new(),
                        ),
                        caption.clone(),
                    ));
            }
            return;
        }
        let arms: Vec<_> = [Arm::Else, Arm::Catch, Arm::Finally]
            .into_iter()
            .filter_map(|arm| Some((arm, spelled.arms.get(&(start, arm))?)))
            .collect();
        if caption.is_none() && arms.is_empty() {
            return;
        }
        let owner = {
            let mut emitted = self.buf.stmts().skip(before);
            match control(statement.unspanned()) {
                Some(control) => emitted.rposition(control),
                None => emitted
                    .position(|stmt| acts(Node::Stmt(stmt)))
                    .or_else(|| self.buf.len().saturating_sub(before).checked_sub(1)),
            }
        };
        let Some((owned, note)) = owner.and_then(|index| self.buf.get_mut(before + index)) else {
            return;
        };
        let indices: Vec<_> = arms
            .into_iter()
            .filter_map(|(arm, caption)| Some((arm_index(owned, arm)?, caption.clone())))
            .collect();
        if let Some(caption) = caption {
            note.caption.get_or_insert_with(|| caption.clone());
        }
        for (index, caption) in indices {
            note.arm_captions.entry(index).or_insert(caption);
        }
    }
}
