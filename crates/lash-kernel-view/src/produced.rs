//! The `perform` a binding holds the reply of (`BindingView::produced_by`).
//!
//! A value is a perform's reply when it is the `perform` itself; the one
//! argument of a function the supplied package view declares receives an
//! effect's reply; an
//! `await` through its dialect's policy of a promise or task started on one
//! closure whose body returns such a value (TypeScript's `ts.promise.start`
//! and Python's `py.task.run`, as [`crate::ActionView::applies`] names the
//! closure); a `let` expression's body; or a variable written once with such
//! a value. A value computed from a reply, such as a field read or a
//! conversion, is no reply: it comes from the code.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_check::{
    ActNode, ActionNode, AtomNode, BindingId, BindingKind, ExprNode, Graph, NodeId, NodeKind,
    Reference, StmtNode,
};

use lash_vocabulary::sites::Site;

use crate::ActionView;

/// What one derivation reads to name a value's producing `perform`.
pub(crate) struct Producers<'a> {
    graph: &'a Graph,
    /// The callables each action of the view applies, by site.
    applies: BTreeMap<&'a Site, &'a [Site]>,
}

impl<'a> Producers<'a> {
    pub(crate) fn new(graph: &'a Graph, actions: &'a [ActionView]) -> Self {
        Self {
            graph,
            applies: actions
                .iter()
                .map(|action| (&action.site, action.applies.as_slice()))
                .collect(),
        }
    }

    /// The site of the `perform` whose reply `binding` holds, when it is a
    /// `let` nothing reassigns.
    pub(crate) fn binding(&self, binding: BindingId) -> Option<Site> {
        let mut seen = BTreeSet::from([binding]);
        let value = self.written_once(binding)?;
        self.reply(value, &mut seen)
    }

    /// The `perform` whose reply the value of `id` is.
    fn reply(&self, id: NodeId, seen: &mut BTreeSet<BindingId>) -> Option<Site> {
        let node = self.graph.node(self.resolve(id, seen)?);
        match &node.kind {
            NodeKind::Action(ActionNode::Perform { .. })
            | NodeKind::Expr(ExprNode::Act {
                kind: ActNode::Perform { .. },
                ..
            }) => Some(node.site.clone()),
            NodeKind::Action(ActionNode::Await { value, .. }) => {
                let value = self.atom(value, seen)?;
                self.settled(value, seen)
            }
            NodeKind::Expr(ExprNode::Act {
                kind: ActNode::Await(_),
                args,
            }) if args.len() == 1 => self.settled(args[0], seen),
            _ => None,
        }
    }

    /// The `perform` whose reply the task an `await` of `id` settles on
    /// returns: a promise or task started on one closure whose whole body
    /// returns it.
    fn settled(&self, id: NodeId, seen: &mut BTreeSet<BindingId>) -> Option<Site> {
        let start = self.graph.node(self.resolve(id, seen)?);
        let [closure] = self.applies.get(&start.site)? else {
            return None;
        };
        let NodeKind::Expr(ExprNode::Closure { body, .. }) = self.graph.node_at(closure)?.kind
        else {
            return None;
        };
        let NodeKind::Block(statements) = &self.graph.node(body).kind else {
            return None;
        };
        let [statement] = statements.as_slice() else {
            return None;
        };
        let NodeKind::Stmt(StmtNode::Return { value }) = self.graph.node(*statement).kind else {
            return None;
        };
        self.reply(value, seen)
    }

    /// The node whose value `id` has: through a `let` expression to its
    /// body, or to its value when the body reads it back, and through a
    /// variable written once to that write.
    fn resolve(&self, mut id: NodeId, seen: &mut BTreeSet<BindingId>) -> Option<NodeId> {
        loop {
            id = match &self.graph.node(id).kind {
                NodeKind::Expr(ExprNode::Let {
                    binding,
                    value,
                    body,
                }) => match &self.graph.node(*body).kind {
                    NodeKind::Expr(ExprNode::Variable(Reference::Binding(read)))
                        if read == binding =>
                    {
                        *value
                    }
                    _ => *body,
                },
                NodeKind::Expr(ExprNode::Variable(Reference::Binding(binding))) => {
                    if !seen.insert(*binding) {
                        return None;
                    }
                    self.written_once(*binding)?
                }
                _ => return Some(id),
            };
        }
    }

    /// The value a statement action's argument reads.
    fn atom(&self, atom: &AtomNode, seen: &mut BTreeSet<BindingId>) -> Option<NodeId> {
        let AtomNode::Variable(Reference::Binding(binding)) = atom else {
            return None;
        };
        if !seen.insert(*binding) {
            return None;
        }
        self.written_once(*binding)
    }

    /// The value of a `let` variable nothing reassigns.
    fn written_once(&self, binding: BindingId) -> Option<NodeId> {
        let binding = self.graph.binding(binding);
        if binding.kind != BindingKind::Let || binding.assigned {
            return None;
        }
        match self.graph.node(binding.declared_at).kind {
            NodeKind::Stmt(StmtNode::Let { value, .. })
            | NodeKind::Expr(ExprNode::Let { value, .. }) => Some(value),
            _ => None,
        }
    }
}
