//! The operand roles of a nested action (`K-EVAL-008`).
//!
//! For an action `A` inside a statement `S`, the live set `L(A)` is a
//! function of the code alone: walking from `S` down to `A`'s parent, each
//! ancestor's children evaluated before the child that holds `A`, each
//! named by that child's site. It is what a frame standing at `A` saves and
//! what a restore checks its saved roles against.
//!
//! The walk follows the evaluation order (`K-EVAL-001`): left siblings in
//! child order, except that a value-first assignment evaluates its
//! right-hand side before its place (`K-EVAL-003`), so the right-hand side
//! is live in the place and nothing of the place is live in the right-hand
//! side, while a place-first one keeps child order: its target and index
//! are live in its right-hand side; and a
//! nested call or `spawn` reads a callee held in a variable before its
//! arguments (`K-EVAL-004`), a role named by the action's own site. The
//! condition of an `if` expression is consumed when its branch is chosen,
//! and the value of a `let` expression is held by its binding, so neither
//! is a role inside the branches or the body.

use crate::ast::ActKind;
use crate::ast::AssignOrder;
use crate::ast::Block;
use crate::ast::Callee;
use crate::ast::Expr;
use crate::ast::Member;
use crate::ast::Node;
use crate::ast::Place;
use crate::ast::Stmt;
use crate::document::Document;
use lash_kernel_value::sites::Site;
use lash_kernel_value::sites::Unit;

/// Why [`live_roles`] gives no answer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LiveRolesError {
    /// The site names no node of the code.
    #[error("the site names no node of the code")]
    NoSuchNode,
    /// The site names a node a frame cannot stand at: neither a nested
    /// action nor a library call inside an expression.
    #[error("the site names no action inside an expression")]
    NotAction,
    /// A library function's code is in no document: its live roles are
    /// [`live_roles_in`] its body.
    #[error("the site names a library body, which is in no document")]
    LibraryUnit,
}

/// The operand roles of the action at `site` in `document`'s own code, in
/// evaluation order: each the site of the child expression whose value the
/// statement already holds when the action is reached.
pub fn live_roles(document: &Document, site: &Site) -> Result<Vec<Site>, LiveRolesError> {
    let body = match &site.unit {
        Unit::Main => document.main.as_ref().ok_or(LiveRolesError::NoSuchNode)?,
        Unit::Library(id) => {
            &document
                .functions
                .get(id)
                .and_then(crate::FunctionDefinition::body)
                .ok_or(LiveRolesError::NoSuchNode)?
                .block
        }
        Unit::Function(_) => return Err(LiveRolesError::NoSuchNode),
    };
    live_roles_in(body, site)
}

/// [`live_roles`] of the action at `site` in `body`, the code of
/// `site`'s unit.
///
/// An action is an [`Expr::Act`], or an [`Expr::Call`]: a library call
/// inside an expression stands in a frame of its own when its function
/// runs its body.
pub fn live_roles_in(body: &Block, site: &Site) -> Result<Vec<Site>, LiveRolesError> {
    // The ancestors of the action: each parent, the child the path takes
    // through it, and the parent's site.
    let mut node = Node::Block(body);
    let mut at = Site::new(site.unit.clone(), []);
    let mut ancestors: Vec<(Node<'_>, u32, Site)> = Vec::with_capacity(site.path.len());
    for &step in &site.path {
        let next = node
            .children()
            .into_iter()
            .nth(step as usize)
            .ok_or(LiveRolesError::NoSuchNode)?;
        ancestors.push((node, step, at.clone()));
        node = next;
        at = at.child(step);
    }
    if !matches!(node, Node::Expr(Expr::Act(_) | Expr::Call { .. })) {
        return Err(LiveRolesError::NotAction);
    }
    // The nearest statement: an action in a closure body is its own
    // statement's, never the enclosing expression's.
    let statement = ancestors
        .iter()
        .rposition(|(parent, _, _)| matches!(parent, Node::Stmt(_)))
        .ok_or(LiveRolesError::NotAction)?;
    let mut roles = Vec::new();
    for (parent, child, parent_site) in &ancestors[statement..] {
        let child = *child;
        let before = match parent {
            Node::Stmt(Stmt::Assign {
                place: Place::Member(member),
                order: AssignOrder::ValueFirst,
                ..
            }) => {
                let value = match member {
                    Member::Field { .. } => 1,
                    Member::Index { .. } => 2,
                };
                if child == value {
                    // The right-hand side comes first: no role of its own
                    // statement is live in it.
                    0..0
                } else {
                    roles.push(parent_site.child(value));
                    0..child
                }
            }
            Node::Expr(Expr::If { .. } | Expr::Let { .. }) => 0..0,
            Node::Expr(Expr::Act(act))
                if matches!(
                    act.kind,
                    ActKind::Call(Callee::Value(_)) | ActKind::Spawn(Callee::Value(_))
                ) =>
            {
                // A callee held in a variable is read before the arguments
                // (`K-EVAL-004`), named by the action's own site.
                roles.push(parent_site.clone());
                0..child
            }
            _ => 0..child,
        };
        let children = parent.children();
        for index in before {
            if matches!(children[index as usize], Node::Expr(_)) {
                roles.push(parent_site.child(index));
            }
        }
    }
    Ok(roles)
}
