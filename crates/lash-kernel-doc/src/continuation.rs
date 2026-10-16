//! The live operand roles of a nested action (`K-EVAL-006`).
//!
//! For an action `A` inside a statement `S`, the live set `L(A)` is a
//! function of the document alone: walking from `S` down to `A`'s parent,
//! each ancestor's children evaluated before the child that contains `A`,
//! each named by that child's site. What a frame parked at `A` saves, and
//! what a restore validates its saved roles against.

use crate::ast::{Expr, Node, Place, Site, Stmt, Unit};
use crate::document::Document;

/// Why [`live_roles`] gives no answer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LiveRolesError {
    /// The site names no node of the document.
    #[error("the site names no node of the document")]
    NoSuchNode,
    /// The site names a node that is not a nested action.
    #[error("the site names a node that is not a nested action")]
    NotAction,
    /// A library unit's code is no document's; its sites belong to its
    /// own function.
    #[error("a library unit has no live roles in a document")]
    LibraryUnit,
}

/// The operand roles of the nested action `action` names, in evaluation
/// order, outer ancestors first: each the site of the child expression
/// that produced the value the statement already holds.
pub fn live_roles(document: &Document, action: &Site) -> Result<Vec<Site>, LiveRolesError> {
    if matches!(action.unit, Unit::Library(_)) {
        return Err(LiveRolesError::LibraryUnit);
    }
    let body = match &action.unit {
        Unit::Main => &document.main,
        Unit::Function(name) => document
            .functions
            .get(name)
            .map(|function| &function.body)
            .ok_or(LiveRolesError::NoSuchNode)?,
        Unit::Library(_) => unreachable!(),
    };
    // The ancestors of the action node: each parent, the child index the
    // path takes through it, and the parent's site.
    let mut node = Node::Block(body);
    let mut site = Site::new(action.unit.clone(), []);
    let mut ancestors: Vec<(Node<'_>, u32, Site)> = Vec::with_capacity(action.path.len());
    for step in &action.path {
        let children = node.children();
        let Some(next) = children.into_iter().nth(*step as usize) else {
            return Err(LiveRolesError::NoSuchNode);
        };
        ancestors.push((node, *step, site.clone()));
        node = next;
        site = site.child(*step);
    }
    if !matches!(node, Node::Expr(Expr::Act(_))) {
        return Err(LiveRolesError::NotAction);
    }
    // The nearest enclosing statement: an action inside a closure body
    // belongs to the closure's statement.
    let Some(enclosing) = ancestors
        .iter()
        .rposition(|(parent, _, _)| matches!(parent, Node::Stmt(_)))
    else {
        return Err(LiveRolesError::NotAction);
    };
    let mut roles = Vec::new();
    for (parent, c, parent_site) in &ancestors[enclosing..] {
        let c = *c as usize;
        let before: usize = match parent {
            // `set place = rhs` evaluates the place after the right-hand
            // side (`K-EVAL-003`), so a nested action in the rhs has no
            // operand roles of its statement.
            Node::Stmt(Stmt::Assign {
                place: Place::Member(member),
                ..
            }) => {
                let place_children = match member {
                    crate::ast::Member::Field { .. } => 1,
                    crate::ast::Member::Index { .. } => 2,
                };
                if c == place_children { 0 } else { c }
            }
            _ => c,
        };
        for (index, child) in parent.children().into_iter().take(before).enumerate() {
            if matches!(child, Node::Expr(_)) {
                roles.push(parent_site.child(index as u32));
            }
        }
    }
    Ok(roles)
}
