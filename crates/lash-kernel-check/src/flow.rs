//! Edges, execution sites and effect sets: what follows from the linked
//! nodes without reading the document again.

use lash_kernel_doc::FunctionTarget;
use lash_vocabulary::names::ImportSlot;
use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{FunctionCatalog, Literal};

use lash_vocabulary::names::FunctionId;

use lash_vocabulary::sites::Unit;

use crate::graph::{
    ActNode, ActionNode, AtomNode, CalleeNode, Edge, EdgeKind, ExecutionSite, ExprNode, Graph,
    NodeId, NodeKind, SiteKind, StmtNode, Target,
};

/// Where each way of leaving a statement leads.
#[derive(Clone, Copy)]
struct Routes {
    next: Target,
    /// `None` outside a loop.
    brk: Option<Target>,
    cont: Option<Target>,
    ret: Target,
    raise: Target,
}

/// Which routes some statement took, other than `next`.
#[derive(Clone, Copy, Default)]
struct Taken {
    brk: bool,
    cont: bool,
    ret: bool,
    raise: bool,
}

impl Taken {
    fn merge(&mut self, other: Taken) {
        self.brk |= other.brk;
        self.cont |= other.cont;
        self.ret |= other.ret;
        self.raise |= other.raise;
    }
}

pub(crate) fn derive(graph: &mut Graph, catalog: &dyn FunctionCatalog) {
    let mut flow = Flow {
        graph,
        edges: Vec::new(),
    };
    flow.run();
    let mut edges = flow.edges;
    edges.sort();
    edges.dedup();
    graph.edges = edges;
    graph.execution_sites = execution_sites(graph);
    effect_sets(graph, catalog);
}

struct Flow<'a> {
    graph: &'a Graph,
    edges: Vec<Edge>,
}

impl Flow<'_> {
    fn run(&mut self) {
        let graph = self.graph;
        let bodies: BTreeMap<&FunctionId, NodeId> = graph
            .units
            .iter()
            .filter_map(|unit| match &unit.unit {
                Unit::Library(id) => Some((id, unit.body)),
                Unit::Main | Unit::Function(_) => None,
            })
            .collect();
        let function_body = |name: &FunctionId| bodies.get(name).copied().map(Target::Node);
        // A function body: every unit's, and every closure's.
        let leave = Routes {
            next: Target::Leave,
            brk: None,
            cont: None,
            ret: Target::Leave,
            raise: Target::Leave,
        };
        for unit in &graph.units {
            self.block(unit.body, leave);
        }
        for node in &graph.nodes {
            let mut referenced: Vec<&FunctionId> = Vec::new();
            match &node.kind {
                NodeKind::Expr(ExprNode::Closure { body, .. }) => {
                    self.block(*body, leave);
                }
                NodeKind::Expr(ExprNode::Literal(Literal::Function(name))) => {
                    referenced.push(name);
                }
                NodeKind::Expr(ExprNode::Act { kind: act, .. }) => {
                    let (callee, kind) = match act {
                        ActNode::Call(callee) => (Some(callee), EdgeKind::Call),
                        ActNode::Spawn(callee) => (Some(callee), EdgeKind::Spawn),
                        _ => (None, EdgeKind::Call),
                    };
                    if let Some(CalleeNode::Local(name)) = callee
                        && let Some(to) = function_body(name)
                    {
                        self.edges.push(Edge {
                            from: node.id,
                            to,
                            kind,
                        });
                    }
                }
                NodeKind::Action(action) => {
                    let (callee, kind) = match action {
                        ActionNode::Call { callee, .. } => (Some(callee), EdgeKind::Call),
                        ActionNode::Spawn { callee, .. } => (Some(callee), EdgeKind::Spawn),
                        _ => (None, EdgeKind::Call),
                    };
                    if let Some(CalleeNode::Local(name)) = callee
                        && let Some(to) = function_body(name)
                    {
                        self.edges.push(Edge {
                            from: node.id,
                            to,
                            kind,
                        });
                    }
                    referenced.extend(atoms(action).filter_map(|atom| match atom {
                        AtomNode::Literal(Literal::Function(name)) => Some(name),
                        _ => None,
                    }));
                }
                _ => {}
            }
            for name in referenced {
                if let Some(to) = function_body(name) {
                    self.edges.push(Edge {
                        from: node.id,
                        to,
                        kind: EdgeKind::Reference,
                    });
                }
            }
        }
    }

    fn edge(&mut self, from: NodeId, to: Target, kind: EdgeKind) {
        self.edges.push(Edge { from, to, kind });
    }

    /// The first statement of a block, or where the block leads when it is
    /// empty.
    fn entry(&self, block: NodeId, next: Target) -> Target {
        match &self.graph.node(block).kind {
            NodeKind::Block(statements) => statements.first().map_or(next, |id| Target::Node(*id)),
            _ => next,
        }
    }

    fn block(&mut self, block: NodeId, routes: Routes) -> Taken {
        let graph = self.graph;
        let NodeKind::Block(statements) = &graph.node(block).kind else {
            return Taken::default();
        };
        let mut taken = Taken::default();
        for (index, stmt) in statements.iter().enumerate() {
            let next = statements
                .get(index + 1)
                .map_or(routes.next, |id| Target::Node(*id));
            taken.merge(self.stmt(*stmt, Routes { next, ..routes }));
        }
        taken
    }

    fn stmt(&mut self, id: NodeId, routes: Routes) -> Taken {
        let graph = self.graph;
        let NodeKind::Stmt(stmt) = &graph.node(id).kind else {
            return Taken::default();
        };
        let mut taken = Taken::default();
        match stmt {
            StmtNode::Let { .. }
            | StmtNode::Assign { .. }
            | StmtNode::Remove { .. }
            | StmtNode::Do { .. }
            | StmtNode::Print { .. } => self.edge(id, routes.next, EdgeKind::Next),
            StmtNode::If {
                then_block,
                else_block,
                ..
            } => {
                self.edge(id, self.entry(*then_block, routes.next), EdgeKind::Then);
                self.edge(id, self.entry(*else_block, routes.next), EdgeKind::Else);
                taken.merge(self.block(*then_block, routes));
                taken.merge(self.block(*else_block, routes));
            }
            StmtNode::For {
                body, else_block, ..
            }
            | StmtNode::While {
                body, else_block, ..
            } => {
                let again = Target::Node(id);
                self.edge(id, self.entry(*body, again), EdgeKind::Body);
                // The loop ends into its `else`; a `break` leaves past it.
                match else_block {
                    Some(else_block) => {
                        self.edge(id, self.entry(*else_block, routes.next), EdgeKind::Else);
                        taken.merge(self.block(*else_block, routes));
                    }
                    None => self.edge(id, routes.next, EdgeKind::Next),
                }
                let inner = self.block(
                    *body,
                    Routes {
                        next: again,
                        brk: Some(routes.next),
                        cont: Some(again),
                        ..routes
                    },
                );
                taken.ret |= inner.ret;
                taken.raise |= inner.raise;
            }
            StmtNode::Break { .. } => {
                if let Some(to) = routes.brk {
                    self.edge(id, to, EdgeKind::Break);
                    taken.brk = true;
                }
            }
            StmtNode::Continue { .. } => {
                if let Some(to) = routes.cont {
                    self.edge(id, to, EdgeKind::Continue);
                    taken.cont = true;
                }
            }
            StmtNode::Return { .. } | StmtNode::ReturnVia { .. } => {
                self.edge(id, routes.ret, EdgeKind::Return);
                taken.ret = true;
            }
            StmtNode::Throw { .. } => {
                self.edge(id, routes.raise, EdgeKind::Throw);
                taken.raise = true;
            }
            StmtNode::Finish { .. } | StmtNode::Fail { .. } => {
                self.edge(id, Target::End, EdgeKind::End);
            }
            StmtNode::Try {
                body,
                catch,
                finally,
            } => {
                // An empty `finally` interrupts nothing.
                let finally = (*finally).filter(|block| {
                    matches!(&graph.node(*block).kind, NodeKind::Block(statements) if !statements.is_empty())
                });
                let inner = match finally {
                    Some(block) => {
                        let entry = self.entry(block, routes.next);
                        self.edge(id, entry, EdgeKind::Finally);
                        Routes {
                            next: entry,
                            brk: routes.brk.map(|_| entry),
                            cont: routes.cont.map(|_| entry),
                            ret: entry,
                            raise: entry,
                        }
                    }
                    None => routes,
                };
                let mut through = Taken::default();
                match catch {
                    Some(catch) => {
                        let handler = self.entry(catch.body, inner.next);
                        self.edge(id, handler, EdgeKind::Catch);
                        let mut body_taken = self.block(
                            *body,
                            Routes {
                                raise: handler,
                                ..inner
                            },
                        );
                        body_taken.raise = false;
                        through.merge(body_taken);
                        through.merge(self.block(catch.body, inner));
                    }
                    None => through.merge(self.block(*body, inner)),
                }
                self.edge(id, self.entry(*body, inner.next), EdgeKind::Body);
                match finally {
                    Some(block) => {
                        // The departures the `finally` interrupted resume
                        // when it completes (`K-FORM-017`).
                        let resumes = [
                            (through.brk, routes.brk, EdgeKind::Break),
                            (through.cont, routes.cont, EdgeKind::Continue),
                            (through.ret, Some(routes.ret), EdgeKind::Return),
                            (through.raise, Some(routes.raise), EdgeKind::Throw),
                        ];
                        for (used, to, kind) in resumes {
                            if let (true, Some(to)) = (used, to) {
                                self.edge(block, to, kind);
                            }
                        }
                        taken.merge(through);
                        taken.merge(self.block(block, routes));
                    }
                    None => taken.merge(through),
                }
            }
        }
        taken
    }
}

pub(crate) fn atoms(action: &ActionNode) -> impl Iterator<Item = &AtomNode> {
    let (many, one): (&[AtomNode], Option<&AtomNode>) = match action {
        ActionNode::Call { args, .. }
        | ActionNode::Spawn { args, .. }
        | ActionNode::Perform { args, .. } => (args, None),
        ActionNode::Join { task: atom }
        | ActionNode::JoinMany { tasks: atom, .. }
        | ActionNode::Cancel { task: atom }
        | ActionNode::Await { value: atom, .. } => (&[], Some(atom)),
        ActionNode::Yield => (&[], None),
    };
    many.iter().chain(one)
}

fn execution_sites(graph: &Graph) -> Vec<ExecutionSite> {
    let mut out = Vec::new();
    for node in &graph.nodes {
        let kind = match &node.kind {
            NodeKind::Action(action) => match action {
                ActionNode::Call { .. } => SiteKind::Call,
                ActionNode::Perform { .. } => SiteKind::Perform,
                ActionNode::Join { .. } => SiteKind::Join,
                ActionNode::JoinMany { .. } => SiteKind::JoinMany,
                ActionNode::Yield => SiteKind::Yield,
                ActionNode::Spawn { .. } => SiteKind::Spawn,
                ActionNode::Cancel { .. } => SiteKind::Cancel,
                ActionNode::Await { .. } => SiteKind::Await,
            },
            NodeKind::Expr(ExprNode::Act { kind, .. }) => match kind {
                ActNode::Call(_) => SiteKind::Call,
                ActNode::Perform { .. } => SiteKind::Perform,
                ActNode::Spawn(_) => SiteKind::Spawn,
                ActNode::Yield => SiteKind::Yield,
                ActNode::Cancel => SiteKind::Cancel,
                ActNode::Join => SiteKind::Join,
                ActNode::JoinMany(_) => SiteKind::JoinMany,
                ActNode::Await(_) => SiteKind::Await,
            },
            _ => continue,
        };
        // A nested action stands in its nearest statement, inside the
        // innermost action it is an argument of, if any (`K-SITE-001`).
        let mut within = None;
        let mut cursor = node.parent;
        let statement = loop {
            let Some(id) = cursor else {
                break None;
            };
            let ancestor = graph.node(id);
            match &ancestor.kind {
                NodeKind::Stmt(_) => break Some(id),
                NodeKind::Expr(ExprNode::Act { .. }) if within.is_none() => {
                    within = Some(ancestor.site.clone());
                }
                _ => {}
            }
            cursor = ancestor.parent;
        };
        let Some(statement) = statement else {
            continue;
        };
        // The loops between the action and its function body, which a
        // closure expression bounds. A `while` evaluates its condition
        // inside its loop, so it encloses an action there (`K-SITE-004`).
        let mut loops = Vec::new();
        if matches!(
            graph.node(statement).kind,
            NodeKind::Stmt(StmtNode::While { .. })
        ) {
            loops.push(graph.node(statement).site.clone());
        }
        let mut cursor = graph.node(statement).parent;
        while let Some(id) = cursor {
            let ancestor = graph.node(id);
            match &ancestor.kind {
                NodeKind::Expr(ExprNode::Closure { .. }) => break,
                NodeKind::Stmt(StmtNode::For { .. } | StmtNode::While { .. }) => {
                    loops.push(ancestor.site.clone());
                }
                _ => {}
            }
            cursor = ancestor.parent;
        }
        loops.reverse();
        out.push(ExecutionSite {
            node: node.id,
            site: node.site.clone(),
            statement: graph.node(statement).site.clone(),
            within,
            kind,
            loops,
        });
    }
    out
}

fn effect_sets(graph: &mut Graph, _catalog: &dyn FunctionCatalog) {
    #[derive(Clone, Default)]
    struct Direct {
        effects: BTreeSet<ImportSlot>,
        reaches: BTreeSet<FunctionId>,
        through_values: bool,
    }
    fn callee(d: &mut Direct, c: &CalleeNode) {
        match c {
            CalleeNode::Local(id) => {
                d.reaches.insert(*id);
            }
            CalleeNode::Import(slot) => {
                d.effects.insert(*slot);
            }
            CalleeNode::Value(_) => d.through_values = true,
        }
    }
    fn target(d: &mut Direct, t: &FunctionTarget) {
        match t {
            FunctionTarget::Local(id) => {
                d.reaches.insert(*id);
            }
            FunctionTarget::Import(slot) => {
                d.effects.insert(*slot);
            }
        }
    }
    let mut direct: BTreeMap<Unit, Direct> = BTreeMap::new();
    for node in &graph.nodes {
        let d = direct.entry(node.site.unit.clone()).or_default();
        match &node.kind {
            NodeKind::Expr(
                ExprNode::Closure { function, .. } | ExprNode::Call { function, .. },
            ) => {
                d.reaches.insert(*function);
            }
            NodeKind::Expr(ExprNode::Literal(Literal::Function(id))) => {
                d.reaches.insert(*id);
            }
            NodeKind::Stmt(StmtNode::ReturnVia { policy, .. }) => target(d, policy),
            NodeKind::Expr(ExprNode::Act { kind, .. }) => match kind {
                ActNode::Call(c) | ActNode::Spawn(c) => callee(d, c),
                ActNode::Perform { slot, .. } => {
                    d.effects.insert(*slot);
                }
                ActNode::Await(policy) => target(d, policy),
                _ => {}
            },
            NodeKind::Action(action) => {
                match action {
                    ActionNode::Call { callee: c, .. } | ActionNode::Spawn { callee: c, .. } => {
                        callee(d, c)
                    }
                    ActionNode::Perform { slot, .. } => {
                        d.effects.insert(*slot);
                    }
                    ActionNode::Await { policy, .. } => target(d, policy),
                    _ => {}
                }
                for atom in atoms(action) {
                    if let AtomNode::Literal(Literal::Function(id)) = atom {
                        d.reaches.insert(*id);
                    }
                }
            }
            _ => {}
        }
    }
    loop {
        let previous = direct.clone();
        let mut changed = false;
        for d in direct.values_mut() {
            for id in &d.reaches {
                if let Some(c) = previous.get(&Unit::Library(*id)) {
                    let before = (d.effects.len(), d.through_values);
                    d.effects.extend(&c.effects);
                    d.through_values |= c.through_values;
                    changed |= before != (d.effects.len(), d.through_values);
                }
            }
        }
        if !changed {
            break;
        }
    }
    for unit in &mut graph.units {
        if let Some(d) = direct.remove(&unit.unit) {
            unit.effects.effects = d.effects;
            unit.effects.through_values = d.through_values;
        }
    }
}
