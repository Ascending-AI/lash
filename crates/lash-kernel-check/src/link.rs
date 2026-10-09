//! Linking: one walk of a unit that numbers its nodes, opens its scopes,
//! declares its variables and resolves every name (`K-FORM-003`,
//! `K-CLO-001`).
//!
//! The walk visits children in the order a site counts them, so a node's id
//! and its site come from the same traversal. It runs only on a tree
//! structural validation has accepted, which bounds its depth.

use lash_kernel_doc::{
    Action, Atom, Block, Callee, Expr, Member, Name, Place, Rhs, Signature, Site, Stmt, Type, Unit,
};

use crate::graph::{
    ActionNode, AtomNode, Binding, BindingId, BindingKind, CalleeNode, CatchNode, EffectSet,
    ExprNode, Graph, GraphNode, MapEntryNode, MemberNode, NodeId, NodeKind, PlaceNode,
    RecordEntryNode, Reference, Scope, ScopeId, StmtNode, UnitView,
};
use crate::refusal::{RefusalReason, Refused, refused};

/// A closure whose body is being walked.
struct ClosureFrame {
    /// The scope of the closure's body. A binding of a scope that opened
    /// earlier is outside the closure.
    scope: ScopeId,
    captures: Vec<Reference>,
}

pub(crate) struct Linker {
    pub(crate) graph: Graph,
    pub(crate) errors: Vec<Refused>,
    /// The unit is `main`: a name nothing declares is a session binding.
    session: bool,
    /// The loops around the statement being walked, within its function
    /// body.
    loops: Vec<NodeId>,
    closures: Vec<ClosureFrame>,
}

/// Counts a node's children as a site does.
struct Slots<'a> {
    site: &'a Site,
    next: u32,
}

impl Slots<'_> {
    fn take(&mut self) -> Site {
        let site = self.site.child(self.next);
        self.next += 1;
        site
    }
}

impl Linker {
    pub(crate) fn new() -> Self {
        Self {
            graph: Graph::empty(),
            errors: Vec::new(),
            session: false,
            loops: Vec::new(),
            closures: Vec::new(),
        }
    }

    pub(crate) fn unit(
        &mut self,
        unit: Unit,
        params: &[Name],
        body: &Block,
        signature: Option<Signature>,
    ) {
        self.session = matches!(unit, Unit::Main);
        let site = Site::new(unit.clone(), []);
        let (body, params) = self.block(body, site, None, None, params, BindingKind::Param, None);
        self.graph.units.push(UnitView {
            unit,
            body,
            params,
            signature,
            effects: EffectSet::default(),
        });
    }

    fn alloc(&mut self, site: Site, parent: Option<NodeId>, scope: ScopeId) -> NodeId {
        let id = NodeId(self.graph.nodes.len() as u32);
        self.graph.sites.insert(site.clone(), id);
        self.graph.nodes.push(GraphNode {
            id,
            site,
            parent,
            scope,
            kind: NodeKind::Block(Vec::new()),
            facet: None,
        });
        id
    }

    fn set(&mut self, id: NodeId, kind: NodeKind) -> NodeId {
        self.graph.nodes[id.index()].kind = kind;
        id
    }

    fn declare(
        &mut self,
        name: &Name,
        kind: BindingKind,
        declared_at: NodeId,
        scope: ScopeId,
    ) -> BindingId {
        let id = BindingId(self.graph.bindings.len() as u32);
        self.graph.bindings.push(Binding {
            id,
            name: name.clone(),
            kind,
            declared_at,
            scope,
            facet: Type::Any,
            assigned: false,
            captured: false,
        });
        self.graph.scopes[scope.index()].bindings.push(id);
        id
    }

    /// Walks a block in a scope of its own, declaring `declare` there first.
    /// `declared_at` is the node that declares them; the block itself when
    /// `None`.
    #[expect(
        clippy::too_many_arguments,
        reason = "one block opens one scope; these are its coordinates"
    )]
    fn block(
        &mut self,
        block: &Block,
        site: Site,
        parent: Option<NodeId>,
        outer: Option<ScopeId>,
        declare: &[Name],
        kind: BindingKind,
        declared_at: Option<NodeId>,
    ) -> (NodeId, Vec<BindingId>) {
        let scope = ScopeId(self.graph.scopes.len() as u32);
        let id = self.alloc(site.clone(), parent, scope);
        self.graph.scopes.push(Scope {
            id: scope,
            parent: outer,
            block: id,
            bindings: Vec::new(),
        });
        let declared = declare
            .iter()
            .map(|name| self.declare(name, kind, declared_at.unwrap_or(id), scope))
            .collect();
        let statements = (0u32..)
            .zip(block)
            .map(|(index, stmt)| self.stmt(stmt, site.child(index), id, scope))
            .collect();
        self.set(id, NodeKind::Block(statements));
        (id, declared)
    }

    fn plain_block(&mut self, block: &Block, site: Site, parent: NodeId, scope: ScopeId) -> NodeId {
        self.block(
            block,
            site,
            Some(parent),
            Some(scope),
            &[],
            BindingKind::Let,
            None,
        )
        .0
    }

    fn stmt(&mut self, stmt: &Stmt, site: Site, parent: NodeId, scope: ScopeId) -> NodeId {
        let id = self.alloc(site.clone(), Some(parent), scope);
        let mut slots = Slots {
            site: &site,
            next: 0,
        };
        let node = match stmt {
            Stmt::Let { name, value } => {
                // The right-hand side is resolved before the name is
                // declared (`K-EVAL-002`).
                let value = self.rhs(value, slots.take(), id, scope);
                let binding = self.declare(name, BindingKind::Let, id, scope);
                StmtNode::Let { binding, value }
            }
            Stmt::Assign { place, value } => {
                let place = match place {
                    Place::Variable(name) => {
                        let reference = self.resolve(name, scope, &site, id);
                        if let Reference::Binding(binding) = &reference {
                            self.graph.bindings[binding.index()].assigned = true;
                        }
                        PlaceNode::Variable(reference)
                    }
                    Place::Member(member) => {
                        PlaceNode::Member(self.member(member, &mut slots, id, scope))
                    }
                };
                let value = self.rhs(value, slots.take(), id, scope);
                StmtNode::Assign { place, value }
            }
            Stmt::Remove { member } => StmtNode::Remove {
                member: self.member(member, &mut slots, id, scope),
            },
            Stmt::Do { action } => StmtNode::Do {
                action: self.action(action, slots.take(), id, scope),
            },
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => StmtNode::If {
                condition: self.expr(condition, slots.take(), id, scope),
                then_block: self.plain_block(then_block, slots.take(), id, scope),
                else_block: self.plain_block(else_block, slots.take(), id, scope),
            },
            Stmt::For {
                binding,
                iterable,
                body,
            } => {
                let iterable = self.expr(iterable, slots.take(), id, scope);
                self.loops.push(id);
                let (body, declared) = self.block(
                    body,
                    slots.take(),
                    Some(id),
                    Some(scope),
                    std::slice::from_ref(binding),
                    BindingKind::For,
                    Some(id),
                );
                self.loops.pop();
                StmtNode::For {
                    binding: declared[0],
                    iterable,
                    body,
                }
            }
            Stmt::While { condition, body } => {
                let condition = self.expr(condition, slots.take(), id, scope);
                self.loops.push(id);
                let body = self.plain_block(body, slots.take(), id, scope);
                self.loops.pop();
                StmtNode::While { condition, body }
            }
            // Validation has refused a `break` or `continue` outside a
            // loop, so there is one.
            Stmt::Break => StmtNode::Break {
                target: self.loops.last().copied().unwrap_or(id),
            },
            Stmt::Continue => StmtNode::Continue {
                target: self.loops.last().copied().unwrap_or(id),
            },
            Stmt::Return { value } => StmtNode::Return {
                value: self.expr(value, slots.take(), id, scope),
            },
            Stmt::Throw { value } => StmtNode::Throw {
                value: self.expr(value, slots.take(), id, scope),
            },
            Stmt::Print { value } => StmtNode::Print {
                value: self.expr(value, slots.take(), id, scope),
            },
            Stmt::Finish { value } => StmtNode::Finish {
                value: self.expr(value, slots.take(), id, scope),
            },
            Stmt::Fail { value } => StmtNode::Fail {
                value: self.expr(value, slots.take(), id, scope),
            },
            Stmt::Try(scoped) => {
                let body = self.plain_block(&scoped.body, slots.take(), id, scope);
                let catch = scoped.catch.as_ref().map(|catch| {
                    let (body, declared) = self.block(
                        &catch.body,
                        slots.take(),
                        Some(id),
                        Some(scope),
                        std::slice::from_ref(&catch.binding),
                        BindingKind::Catch,
                        Some(id),
                    );
                    CatchNode {
                        binding: declared[0],
                        body,
                    }
                });
                let finally = scoped
                    .finally
                    .as_ref()
                    .map(|finally| self.plain_block(finally, slots.take(), id, scope));
                StmtNode::Try {
                    body,
                    catch,
                    finally,
                }
            }
        };
        self.set(id, NodeKind::Stmt(node))
    }

    fn rhs(&mut self, rhs: &Rhs, site: Site, parent: NodeId, scope: ScopeId) -> NodeId {
        match rhs {
            Rhs::Expr(expr) => self.expr(expr, site, parent, scope),
            Rhs::Action(action) => self.action(action, site, parent, scope),
        }
    }

    fn member(
        &mut self,
        member: &Member,
        slots: &mut Slots<'_>,
        parent: NodeId,
        scope: ScopeId,
    ) -> MemberNode {
        match member {
            Member::Field { target, field } => MemberNode::Field {
                target: self.expr(target, slots.take(), parent, scope),
                field: field.clone(),
            },
            Member::Index { target, index } => MemberNode::Index {
                target: self.expr(target, slots.take(), parent, scope),
                index: self.expr(index, slots.take(), parent, scope),
            },
        }
    }

    fn action(&mut self, action: &Action, site: Site, parent: NodeId, scope: ScopeId) -> NodeId {
        let id = self.alloc(site.clone(), Some(parent), scope);
        let node = match action {
            Action::Call { callee, args } => {
                let (callee, args) = self.call(callee, args, scope, &site, id);
                ActionNode::Call { callee, args }
            }
            Action::Spawn { callee, args } => {
                let (callee, args) = self.call(callee, args, scope, &site, id);
                ActionNode::Spawn { callee, args }
            }
            Action::Perform {
                effect,
                args,
                result,
            } => ActionNode::Perform {
                effect: effect.clone(),
                args: self.atoms(args, scope, &site, id),
                result: result.clone(),
            },
            Action::Sleep { duration } => ActionNode::Sleep {
                duration: self.atom(duration, scope, &site, id),
            },
            Action::Join { task } => ActionNode::Join {
                task: self.atom(task, scope, &site, id),
            },
            Action::JoinMany { mode, tasks } => ActionNode::JoinMany {
                mode: *mode,
                tasks: self.atom(tasks, scope, &site, id),
            },
            Action::Yield => ActionNode::Yield,
            Action::Cancel { task } => ActionNode::Cancel {
                task: self.atom(task, scope, &site, id),
            },
        };
        self.set(id, NodeKind::Action(node))
    }

    fn atom(&mut self, atom: &Atom, scope: ScopeId, site: &Site, node: NodeId) -> AtomNode {
        match atom {
            Atom::Variable(name) => AtomNode::Variable(self.resolve(name, scope, site, node)),
            Atom::Literal(literal) => AtomNode::Literal(literal.clone()),
        }
    }

    fn atoms(
        &mut self,
        atoms: &[Atom],
        scope: ScopeId,
        site: &Site,
        node: NodeId,
    ) -> Vec<AtomNode> {
        atoms
            .iter()
            .map(|atom| self.atom(atom, scope, site, node))
            .collect()
    }

    fn call(
        &mut self,
        callee: &Callee,
        args: &[Atom],
        scope: ScopeId,
        site: &Site,
        node: NodeId,
    ) -> (CalleeNode, Vec<AtomNode>) {
        // The callee is read before the arguments (`K-EVAL-004`).
        let callee = match callee {
            Callee::Declared(name) => CalleeNode::Declared(name.clone()),
            Callee::Value(name) => CalleeNode::Value(self.resolve(name, scope, site, node)),
            Callee::Library(function) => CalleeNode::Library(*function),
        };
        (callee, self.atoms(args, scope, site, node))
    }

    fn exprs<'e>(
        &mut self,
        exprs: impl Iterator<Item = &'e Expr>,
        slots: &mut Slots<'_>,
        parent: NodeId,
        scope: ScopeId,
    ) -> Vec<NodeId> {
        exprs
            .map(|expr| self.expr(expr, slots.take(), parent, scope))
            .collect()
    }

    fn expr(&mut self, expr: &Expr, site: Site, parent: NodeId, scope: ScopeId) -> NodeId {
        let id = self.alloc(site.clone(), Some(parent), scope);
        let mut slots = Slots {
            site: &site,
            next: 0,
        };
        let node = match expr {
            Expr::Literal(literal) => ExprNode::Literal(literal.clone()),
            Expr::Variable(name) => ExprNode::Variable(self.resolve(name, scope, &site, id)),
            Expr::Tuple(items) => ExprNode::Tuple(self.exprs(items.iter(), &mut slots, id, scope)),
            Expr::List(items) => ExprNode::List(self.exprs(items.iter(), &mut slots, id, scope)),
            Expr::Set(items) => ExprNode::Set(self.exprs(items.iter(), &mut slots, id, scope)),
            Expr::Map(entries) => ExprNode::Map(
                entries
                    .iter()
                    .map(|entry| MapEntryNode {
                        key: self.expr(&entry.key, slots.take(), id, scope),
                        value: self.expr(&entry.value, slots.take(), id, scope),
                    })
                    .collect(),
            ),
            Expr::Record(entries) => ExprNode::Record(
                entries
                    .iter()
                    .map(|entry| RecordEntryNode {
                        field: entry.field.clone(),
                        value: self.expr(&entry.value, slots.take(), id, scope),
                    })
                    .collect(),
            ),
            Expr::Member(member) => ExprNode::Member(self.member(member, &mut slots, id, scope)),
            Expr::Closure(closure) => {
                // A closure body is a function body: the loops around the
                // closure do not reach into it (`K-CLO-003`).
                let loops = std::mem::take(&mut self.loops);
                self.closures.push(ClosureFrame {
                    scope: ScopeId(self.graph.scopes.len() as u32),
                    captures: Vec::new(),
                });
                let (body, params) = self.block(
                    &closure.body,
                    slots.take(),
                    Some(id),
                    Some(scope),
                    &closure.params,
                    BindingKind::Param,
                    Some(id),
                );
                self.loops = loops;
                let captures = self
                    .closures
                    .pop()
                    .map(|frame| frame.captures)
                    .unwrap_or_default();
                ExprNode::Closure {
                    params,
                    body,
                    captures,
                }
            }
            Expr::Call { function, args } => ExprNode::Call {
                function: *function,
                args: self.exprs(args.iter(), &mut slots, id, scope),
            },
            Expr::Clock => ExprNode::Clock,
            Expr::Random => ExprNode::Random,
            Expr::Read(read) => ExprNode::Read {
                handle: self.expr(&read.handle, slots.take(), id, scope),
                request: self.expr(&read.request, slots.take(), id, scope),
            },
        };
        self.set(id, NodeKind::Expr(node))
    }

    /// Resolves `name` as it stands at `node`: the latest declaration in the
    /// innermost enclosing scope that has one by now.
    fn resolve(&mut self, name: &Name, scope: ScopeId, site: &Site, node: NodeId) -> Reference {
        let mut cursor = Some(scope);
        let mut found = None;
        while let Some(id) = cursor {
            let scope = &self.graph.scopes[id.index()];
            found = scope
                .bindings
                .iter()
                .rev()
                .copied()
                .find(|binding| &self.graph.bindings[binding.index()].name == name);
            if found.is_some() {
                break;
            }
            cursor = scope.parent;
        }
        let (reference, declared_in) = match found {
            Some(binding) => (
                Reference::Binding(binding),
                Some(self.graph.bindings[binding.index()].scope),
            ),
            None => {
                if self.session {
                    self.graph
                        .session_reads
                        .entry(name.clone())
                        .or_default()
                        .push(node);
                } else {
                    self.errors.push(refused(
                        Some(site),
                        RefusalReason::UnboundVariable { name: name.clone() },
                    ));
                }
                (Reference::Session(name.clone()), None)
            }
        };
        // Every closure being walked whose body opened after the variable's
        // scope shares the variable.
        let mut captured = false;
        for frame in &mut self.closures {
            if declared_in.is_none_or(|declared_in| declared_in < frame.scope) {
                captured = true;
                if !frame.captures.contains(&reference) {
                    frame.captures.push(reference.clone());
                }
            }
        }
        if let (true, Reference::Binding(binding)) = (captured, &reference) {
            self.graph.bindings[binding.index()].captured = true;
        }
        reference
    }
}
