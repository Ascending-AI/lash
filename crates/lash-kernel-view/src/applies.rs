//! The callables an action runs that the view can derive (`ActionView::applies`).
//!
//! A call or `spawn` through a variable runs what the variable holds. When
//! every write of the variable that can hold a callable writes one closure
//! or one declared function reference, directly or through another such
//! variable, that is the callable: a literal written beside it, such as the
//! `absent` a hoisted declaration starts with, cannot be applied. A library
//! function runs a closure it is passed when its kernel body calls or
//! spawns through that parameter, or passes it on where another library
//! function does (`ts.promise.start` hands its `work` to `ts.promise.run`,
//! which applies it). A parameter the body binds again is not followed, and
//! a native-only function applies nothing the view can see.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_check::{
    ActNode, ActionNode, AtomNode, BindingId, CalleeNode, ExprNode, Graph, NodeId, NodeKind,
    PlaceNode, Reference, StmtNode,
};
use lash_kernel_doc::{
    ActKind, Action, Atom, Callee, Expr, FunctionDeclarations, FunctionId, Literal, Name, Node,
    Place, Site, Stmt, Unit,
};

use crate::Derived;

/// What one derivation has learnt about its graph and its library.
pub(crate) struct Links {
    /// The value of every write of each `let` variable: its declaration's,
    /// then each assignment's.
    writes: BTreeMap<BindingId, Vec<NodeId>>,
    /// What each library function's body applies, by parameter index.
    applied: BTreeMap<FunctionId, BTreeSet<usize>>,
}

/// What a value is, as a callee.
#[derive(Clone, PartialEq, Eq)]
enum Held {
    /// A value no call can apply: a literal other than a function reference.
    Inert,
    Callable(Site),
    Unknown,
}

impl Links {
    pub(crate) fn new(graph: &Graph) -> Self {
        let mut writes: BTreeMap<BindingId, Vec<NodeId>> = BTreeMap::new();
        for node in graph.nodes() {
            match &node.kind {
                NodeKind::Stmt(StmtNode::Let { binding, value })
                | NodeKind::Expr(ExprNode::Let { binding, value, .. })
                | NodeKind::Stmt(StmtNode::Assign {
                    place: PlaceNode::Variable(Reference::Binding(binding)),
                    value,
                    ..
                }) => writes.entry(*binding).or_default().push(*value),
                _ => {}
            }
        }
        Self {
            writes,
            applied: BTreeMap::new(),
        }
    }

    /// The callables `action` runs, in argument order after a value callee.
    pub(crate) fn applies(
        &mut self,
        graph: &Graph,
        functions: &FunctionDeclarations,
        action: Derived<'_>,
    ) -> Vec<Site> {
        let (callee, arguments): (Option<&CalleeNode>, Vec<Held>) = match action {
            Derived::Statement(
                ActionNode::Call { callee, args } | ActionNode::Spawn { callee, args },
            ) => (
                Some(callee),
                args.iter()
                    .map(|atom| match atom {
                        AtomNode::Variable(reference) => self.variable(graph, reference),
                        AtomNode::Literal(literal) => literal_held(literal),
                    })
                    .collect(),
            ),
            Derived::Nested(ActNode::Call(callee) | ActNode::Spawn(callee), args) => (
                Some(callee),
                args.iter()
                    .map(|arg| self.value(graph, *arg, &mut BTreeSet::new()))
                    .collect(),
            ),
            Derived::Statement(_) | Derived::Nested(..) => (None, Vec::new()),
        };
        let held = match callee {
            Some(CalleeNode::Value(reference)) => vec![self.variable(graph, reference)],
            Some(CalleeNode::Library(function)) => {
                let applied = applied(functions, function, &mut self.applied);
                arguments
                    .into_iter()
                    .enumerate()
                    .filter(|(index, _)| applied.contains(index))
                    .map(|(_, held)| held)
                    .collect()
            }
            Some(CalleeNode::Declared(_)) | None => Vec::new(),
        };
        held.into_iter()
            .filter_map(|held| match held {
                Held::Callable(site) => Some(site),
                Held::Inert | Held::Unknown => None,
            })
            .collect()
    }

    fn variable(&self, graph: &Graph, reference: &Reference) -> Held {
        match reference {
            Reference::Binding(binding) => self.binding(graph, *binding, &mut BTreeSet::new()),
            Reference::Session(_) => Held::Unknown,
        }
    }

    /// What a variable holds whenever a call applies it. `seen` cuts a
    /// cycle of variables written from one another.
    fn binding(&self, graph: &Graph, binding: BindingId, seen: &mut BTreeSet<BindingId>) -> Held {
        if graph.binding(binding).kind != lash_kernel_check::BindingKind::Let
            || !seen.insert(binding)
        {
            return Held::Unknown;
        }
        let mut held = Held::Inert;
        for value in self.writes.get(&binding).into_iter().flatten() {
            held = match (held, self.value(graph, *value, seen)) {
                (Held::Unknown, _) | (_, Held::Unknown) => return Held::Unknown,
                (held, Held::Inert) => held,
                (Held::Inert, callable) => callable,
                (Held::Callable(site), Held::Callable(other)) if site == other => {
                    Held::Callable(site)
                }
                (Held::Callable(_), Held::Callable(_)) => return Held::Unknown,
            };
        }
        held
    }

    /// What the value of an expression is, as a callee.
    fn value(&self, graph: &Graph, value: NodeId, seen: &mut BTreeSet<BindingId>) -> Held {
        let node = graph.node(value);
        match &node.kind {
            NodeKind::Expr(ExprNode::Closure { .. }) => Held::Callable(node.site.clone()),
            NodeKind::Expr(ExprNode::Literal(literal)) => literal_held(literal),
            NodeKind::Expr(ExprNode::Variable(Reference::Binding(binding))) => {
                self.binding(graph, *binding, seen)
            }
            _ => Held::Unknown,
        }
    }
}

/// A declared function reference is that function's body, as
/// `derive_callable` opens it; no other literal can be applied.
fn literal_held(literal: &Literal) -> Held {
    match literal {
        Literal::Function(name) => {
            Held::Callable(Site::new(Unit::Function(name.clone()), Vec::new()))
        }
        _ => Held::Inert,
    }
}

/// The parameters of `function`, by index, its kernel body may apply.
/// Identities are content hashes, so library calls form no cycle.
fn applied(
    functions: &FunctionDeclarations,
    function: &FunctionId,
    known: &mut BTreeMap<FunctionId, BTreeSet<usize>>,
) -> BTreeSet<usize> {
    if let Some(applied) = known.get(function) {
        return applied.clone();
    }
    let mut applied = BTreeSet::new();
    if let Some(definition) = functions.get(function)
        && let Some(body) = definition.body()
    {
        let root = Node::Block(&body.block);
        let rebound = bound(root);
        let params: Vec<Option<&Name>> = definition
            .signature
            .params
            .iter()
            .map(|param| Some(&param.name).filter(|name| !rebound.contains(*name)))
            .collect();
        let index = |name: &Name| params.iter().position(|param| *param == Some(name));
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            let (callee, args): (&Callee, Vec<Option<&Name>>) = match node {
                Node::Action(Action::Call { callee, args } | Action::Spawn { callee, args }) => (
                    callee,
                    args.iter()
                        .map(|atom| match atom {
                            Atom::Variable(name) => Some(name),
                            Atom::Literal(_) => None,
                        })
                        .collect(),
                ),
                Node::Expr(Expr::Act(act)) => match &act.kind {
                    ActKind::Call(callee) | ActKind::Spawn(callee) => (
                        callee,
                        act.args
                            .iter()
                            .map(|arg| match arg {
                                Expr::Variable(name) => Some(name),
                                _ => None,
                            })
                            .collect(),
                    ),
                    _ => {
                        pending.extend(node.children());
                        continue;
                    }
                },
                _ => {
                    pending.extend(node.children());
                    continue;
                }
            };
            match callee {
                Callee::Value(name) => applied.extend(index(name)),
                Callee::Library(callee) => {
                    let inner = self::applied(functions, callee, known);
                    applied.extend(
                        args.iter()
                            .enumerate()
                            .filter(|(at, _)| inner.contains(at))
                            .filter_map(|(_, name)| name.and_then(index)),
                    );
                }
                Callee::Declared(_) => {}
            }
            pending.extend(node.children());
        }
    }
    known.insert(*function, applied.clone());
    applied
}

/// Every name a body binds or assigns, at any depth.
fn bound(root: Node<'_>) -> BTreeSet<Name> {
    let mut names = BTreeSet::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        match node {
            Node::Stmt(
                Stmt::Let { name, .. }
                | Stmt::For { binding: name, .. }
                | Stmt::Assign {
                    place: Place::Variable(name),
                    ..
                },
            )
            | Node::Expr(Expr::Let { name, .. }) => {
                names.insert(name.clone());
            }
            Node::Stmt(Stmt::Try(attempt)) => {
                names.extend(attempt.catch.iter().map(|catch| catch.binding.clone()));
            }
            Node::Expr(Expr::Closure(closure)) => names.extend(closure.params.iter().cloned()),
            _ => {}
        }
        pending.extend(node.children());
    }
    names
}
