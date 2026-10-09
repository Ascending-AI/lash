//! Edges, execution sites and effect sets: what follows from the linked
//! nodes without reading the document again.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{EffectName, FunctionCatalog, Literal, Name, Unit};

use crate::graph::{
    ActionNode, AtomNode, CalleeNode, Edge, EdgeKind, ExecutionSite, ExprNode, Graph, NodeId,
    NodeKind, SiteKind, StmtNode, Target,
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
        let bodies: BTreeMap<&Name, NodeId> = graph
            .units
            .iter()
            .filter_map(|unit| match &unit.unit {
                Unit::Function(name) => Some((name, unit.body)),
                Unit::Main | Unit::Library(_) => None,
            })
            .collect();
        let function_body = |name: &Name| bodies.get(name).copied().map(Target::Node);
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
            let mut referenced: Vec<&Name> = Vec::new();
            match &node.kind {
                NodeKind::Expr(ExprNode::Closure { body, .. }) => {
                    self.block(*body, leave);
                }
                NodeKind::Expr(ExprNode::Literal(Literal::Function(name))) => {
                    referenced.push(name);
                }
                NodeKind::Action(action) => {
                    let (callee, kind) = match action {
                        ActionNode::Call { callee, .. } => (Some(callee), EdgeKind::Call),
                        ActionNode::Spawn { callee, .. } => (Some(callee), EdgeKind::Spawn),
                        _ => (None, EdgeKind::Call),
                    };
                    if let Some(CalleeNode::Declared(name)) = callee
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
            StmtNode::For { body, .. } | StmtNode::While { body, .. } => {
                let again = Target::Node(id);
                self.edge(id, self.entry(*body, again), EdgeKind::Body);
                self.edge(id, routes.next, EdgeKind::Next);
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
            StmtNode::Return { .. } => {
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

fn atoms(action: &ActionNode) -> impl Iterator<Item = &AtomNode> {
    let (many, one): (&[AtomNode], Option<&AtomNode>) = match action {
        ActionNode::Call { args, .. }
        | ActionNode::Spawn { args, .. }
        | ActionNode::Perform { args, .. } => (args, None),
        ActionNode::Sleep { duration: atom }
        | ActionNode::Join { task: atom }
        | ActionNode::JoinMany { tasks: atom, .. }
        | ActionNode::Cancel { task: atom } => (&[], Some(atom)),
        ActionNode::Yield => (&[], None),
    };
    many.iter().chain(one)
}

fn execution_sites(graph: &Graph) -> Vec<ExecutionSite> {
    let mut out = Vec::new();
    for node in &graph.nodes {
        let NodeKind::Action(action) = &node.kind else {
            continue;
        };
        let Some(statement) = node.parent else {
            continue;
        };
        // The loops between the action and its function body, which a
        // closure expression bounds.
        let mut loops = Vec::new();
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
            kind: match action {
                ActionNode::Call { .. } => SiteKind::Call,
                ActionNode::Perform { .. } => SiteKind::Perform,
                ActionNode::Sleep { .. } => SiteKind::Sleep,
                ActionNode::Join { .. } => SiteKind::Join,
                ActionNode::JoinMany { .. } => SiteKind::JoinMany,
                ActionNode::Yield => SiteKind::Yield,
                ActionNode::Spawn { .. } => SiteKind::Spawn,
                ActionNode::Cancel { .. } => SiteKind::Cancel,
            },
            loops,
        });
    }
    out
}

/// What one unit's own code does, before following its calls.
#[derive(Default)]
struct Direct {
    effects: BTreeSet<EffectName>,
    reaches: BTreeSet<Name>,
    through_values: bool,
}

/// Fills each unit's effect set: its own performs, then those of every
/// declared function it calls, spawns or references, to a fixed point.
fn effect_sets(graph: &mut Graph, catalog: &dyn FunctionCatalog) {
    let mut direct: BTreeMap<Unit, Direct> = BTreeMap::new();
    for node in &graph.nodes {
        let unit = direct.entry(node.site.unit.clone()).or_default();
        match &node.kind {
            NodeKind::Expr(ExprNode::Literal(Literal::Function(name))) => {
                unit.reaches.insert(name.clone());
            }
            NodeKind::Action(action) => {
                if let ActionNode::Perform { effect, .. } = action {
                    unit.effects.insert(effect.clone());
                }
                if let ActionNode::Call { callee, .. } | ActionNode::Spawn { callee, .. } = action {
                    match callee {
                        CalleeNode::Declared(name) => {
                            unit.reaches.insert(name.clone());
                        }
                        CalleeNode::Value(_) => unit.through_values = true,
                        CalleeNode::Library(function) => {
                            unit.through_values |= catalog
                                .definition(function)
                                .is_none_or(|definition| !definition.has_native());
                        }
                    }
                }
                for atom in atoms(action) {
                    if let AtomNode::Literal(Literal::Function(name)) = atom {
                        unit.reaches.insert(name.clone());
                    }
                }
            }
            _ => {}
        }
    }
    loop {
        let mut changed = false;
        let units: Vec<Unit> = direct.keys().cloned().collect();
        for unit in units {
            let reaches: Vec<Name> = direct[&unit].reaches.iter().cloned().collect();
            for name in reaches {
                let Some(callee) = direct.get(&Unit::Function(name)) else {
                    continue;
                };
                let effects = callee.effects.clone();
                let through_values = callee.through_values;
                let Some(caller) = direct.get_mut(&unit) else {
                    continue;
                };
                let before = (caller.effects.len(), caller.through_values);
                caller.effects.extend(effects);
                caller.through_values |= through_values;
                changed |= before != (caller.effects.len(), caller.through_values);
            }
        }
        if !changed {
            break;
        }
    }
    for view in &mut graph.units {
        if let Some(found) = direct.remove(&view.unit) {
            view.effects.effects = found.effects;
            view.effects.through_values = found.through_values;
        }
    }
}
