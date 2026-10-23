//! Rewrite resolved uses without confusing equal spellings in different scopes.
use crate::{
    ActNode, ActionNode, AtomNode, BindingId, CalleeNode, ExprNode, Graph, NodeKind, PlaceNode,
    Reference, StmtNode,
};
use lash_kernel_doc::{ActKind, Action, Atom, Callee, Document, Expr, NodeMut, Place};
use lash_vocabulary::{Name, sites::Unit};

/// Change only reads and writes resolved to `binding` in `graph`.
/// The document must still have the graph's tree and function identities.
pub fn rename_binding_uses(
    document: &mut Document,
    graph: &Graph,
    binding: BindingId,
    name: &Name,
) {
    for checked in graph.nodes() {
        let block = match &checked.site.unit {
            Unit::Main => document.main.as_mut(),
            Unit::Library(id) => document
                .functions
                .get_mut(id)
                .and_then(|f| f.body_mut())
                .map(|b| &mut b.block),
            Unit::Function(_) => None,
        };
        let Some(node) = block.and_then(|b| NodeMut::Block(b).descend(&checked.site.path)) else {
            continue;
        };
        let references = binding_references(&checked.kind);
        for (written, resolved) in written_references(node).into_iter().zip(references) {
            if *resolved == Reference::Binding(binding) {
                *written = name.clone();
            }
        }
    }
}

/// The ordered lexical uses owned by one checked node.
pub fn binding_references(kind: &NodeKind) -> Vec<&Reference> {
    let mut out = Vec::new();
    fn callee(c: &CalleeNode) -> Option<&Reference> {
        if let CalleeNode::Value(r) = c {
            Some(r)
        } else {
            None
        }
    }
    match kind {
        NodeKind::Expr(ExprNode::Variable(r))
        | NodeKind::Stmt(StmtNode::Assign {
            place: PlaceNode::Variable(r),
            ..
        }) => out.push(r),
        NodeKind::Expr(ExprNode::Act {
            kind: ActNode::Call(c) | ActNode::Spawn(c),
            ..
        }) => out.extend(callee(c)),
        NodeKind::Action(action) => {
            let atoms: Vec<_> = match action {
                ActionNode::Call { callee: c, args } | ActionNode::Spawn { callee: c, args } => {
                    out.extend(callee(c));
                    args.iter().collect()
                }
                ActionNode::Perform { args, .. } => args.iter().collect(),
                ActionNode::Join { task } | ActionNode::Cancel { task } => vec![task],
                ActionNode::JoinMany { tasks, .. } => vec![tasks],
                ActionNode::Await { value, .. } => vec![value],
                ActionNode::Yield => vec![],
            };
            out.extend(atoms.into_iter().filter_map(|a| {
                if let AtomNode::Variable(r) = a {
                    Some(r)
                } else {
                    None
                }
            }));
        }
        _ => {}
    }
    out
}
fn written_references(node: NodeMut<'_>) -> Vec<&mut Name> {
    let mut out = Vec::new();
    fn callee(c: &mut Callee) -> Option<&mut Name> {
        if let Callee::Value(n) = c {
            Some(n)
        } else {
            None
        }
    }
    match node {
        NodeMut::Expr(Expr::Variable(n))
        | NodeMut::Stmt(lash_kernel_doc::Stmt::Assign {
            place: Place::Variable(n),
            ..
        }) => out.push(n),
        NodeMut::Expr(Expr::Act(act)) => {
            if let ActKind::Call(c) | ActKind::Spawn(c) = &mut act.kind {
                out.extend(callee(c));
            }
        }
        NodeMut::Action(action) => {
            let atoms: Vec<_> = match action {
                Action::Call { callee: c, args } | Action::Spawn { callee: c, args } => {
                    out.extend(callee(c));
                    args.iter_mut().collect()
                }
                Action::Perform { args, .. } => args.iter_mut().collect(),
                Action::Join { task } | Action::Cancel { task } => vec![task],
                Action::JoinMany { tasks, .. } => vec![tasks],
                Action::Await { value, .. } => vec![value],
                Action::Yield => vec![],
            };
            out.extend(atoms.into_iter().filter_map(|a| {
                if let Atom::Variable(n) = a {
                    Some(n)
                } else {
                    None
                }
            }));
        }
        _ => {}
    }
    out
}
