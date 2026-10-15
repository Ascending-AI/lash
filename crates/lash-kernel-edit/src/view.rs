//! The canvas read of one entry of a document: what a host draws and edits
//! against, with no presentation.
//!
//! [`derive`] projects a document and the [`Graph`] the checker derived from
//! it into a [`WorkflowView`]: the entry's statements with their exact sites
//! and payloads, the blocks a host can select with what opens each, the
//! dataflow a host draws ports and wires from, and every execution site a
//! run reports at. The projection is one walk of the entry's graph nodes
//! beside their document nodes, each in [`Node::children`] order, and it
//! matches every statement and expression kind: a new kernel form fails to
//! compile here, not in a host.
//!
//! The view is data. It serialises, and its schema is the
//! `kernel-workflow-view` host schema. Ids name bindings within one
//! derivation; edits address sites, never these ids. Labels, layout,
//! styling and templates are the host's.

use std::collections::BTreeMap;

use lash_kernel_check::{
    ActionNode, AtomNode, BindingId, CalleeNode, ExprNode, Graph, GraphNode, NodeId, NodeKind,
    Reference, StmtNode,
};
use lash_kernel_doc::{
    Action, Atom, Callee, Document, Expr, FunctionCatalog, FunctionId, FunctionName, Name, Node,
    Param, Place, Site, Stmt, Type, Unit,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One entry of a document as a canvas reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowView {
    /// The entry's statements in document order, nested blocks after the
    /// statement that holds them.
    pub statements: Vec<StatementView>,
    /// Every block of the entry, including empty ones, in document order.
    pub scopes: Vec<ScopeView>,
    pub dataflow: Dataflow,
    /// Every action of the document a run can report an occurrence at, in
    /// node order. An entry may call and spawn any declared function, so
    /// these are the sites of every unit.
    pub execution_sites: Vec<ExecutionSiteView>,
}

/// One statement of the entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatementView {
    pub site: Site,
    /// The statement as the document writes it.
    pub statement: Stmt,
    /// The block the statement is in.
    pub block: Site,
    /// How many statements enclose this one inside the entry's body.
    pub depth: usize,
    /// The site of the statement's action, when it has one: where a run
    /// reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<Site>,
}

/// One block of the entry and what opens it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeView {
    pub block: Site,
    pub kind: ScopeKind,
}

/// What a block is the body of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    /// The entry function's own body.
    Entry,
    /// A closure's body.
    Closure,
    /// An `if`'s block taken when its condition holds.
    Then,
    /// An `if`'s block taken when its condition does not hold.
    Else,
    /// A `for` loop's body.
    For,
    /// A `while` loop's body.
    While,
    /// A `try`'s guarded body.
    Try,
    /// A `try`'s `catch` body.
    Catch,
    /// A `try`'s `finally` block.
    Finally,
}

/// What a variable is and where values flow in the entry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Dataflow {
    /// Every variable the entry declares, in declaration order.
    pub bindings: Vec<BindingView>,
    /// Every expression of the entry, in document order.
    pub expressions: Vec<ExpressionView>,
    /// Every action of the entry, in document order.
    pub actions: Vec<ActionView>,
    /// The entry's parameters, by binding id.
    pub parameters: Vec<usize>,
    /// Every `return` and `finish` of the entry and the callable it ends.
    pub returns: Vec<ReturnView>,
    /// Every assignment of the entry.
    pub assignments: Vec<AssignmentView>,
}

/// One variable declaration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BindingView {
    /// The binding's id in this derivation.
    pub id: usize,
    pub name: Name,
    pub kind: BindingKind,
    /// The node that declares it.
    pub site: Site,
    /// The block it is declared in.
    pub block: Site,
    /// The checker's facet of what it holds.
    #[serde(rename = "type")]
    pub ty: Type,
    /// Some statement assigns it after its declaration.
    pub assigned: bool,
    /// A closure shares it.
    pub captured: bool,
    /// The site of a `let`'s value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_site: Option<Site>,
    /// Every node that reads it, in document order.
    pub uses: Vec<Site>,
}

/// How a variable is declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BindingKind {
    Param,
    Let,
    For,
    Catch,
}

/// One expression.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExpressionView {
    pub site: Site,
    /// The statement the expression belongs to.
    pub statement: Site,
    pub block: Site,
    pub expression: Expr,
    /// The checker's facet of its value.
    #[serde(rename = "type")]
    pub ty: Type,
    /// The bindings it reads, by id. A closure's body is its own.
    pub references: Vec<usize>,
    /// The bindings a name at this site resolves to, by id: one per name,
    /// as lexical lookup finds it. A binding's own initializer cannot read
    /// it.
    pub available: Vec<usize>,
}

/// One action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionView {
    pub site: Site,
    pub statement: Site,
    pub block: Site,
    /// What an assignment of the action's result writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<Place>,
    /// The statically named target of a library or declared call or spawn.
    /// Calls through values have no static callee name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callee: Option<CalleeView>,
    pub args: Vec<ArgumentView>,
    /// A `perform`'s declared result, else the checker's facet.
    pub result_type: Type,
    pub available: Vec<usize>,
}

/// A statically named call target. Declared names are arbitrary kernel names;
/// library names are qualified [`FunctionName`]s.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CalleeView {
    Library(FunctionName),
    Declared(Name),
}

/// One argument of an action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArgumentView {
    /// The parameter the library, declared function or effect names here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<Name>,
    /// That parameter's type; `any` for an untyped declared parameter or
    /// when no signature names one.
    #[serde(rename = "type")]
    pub ty: Type,
    pub atom: Atom,
    /// The binding a variable argument reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<usize>,
}

/// One `return` or `finish`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReturnView {
    /// What it ends: the enclosing closure, or the entry (a `finish`
    /// always ends the entry).
    pub callable: Site,
    /// The result type that callable declares.
    pub expected_type: Type,
    pub statement: Site,
    pub expression_site: Site,
    /// The checker's facet of the value.
    #[serde(rename = "type")]
    pub ty: Type,
}

/// One assignment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssignmentView {
    pub statement: Site,
    pub value_site: Site,
    pub destination: Place,
}

/// One action and the coordinates it runs under.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSiteView {
    /// The action's site.
    pub site: Site,
    /// The statement whose right-hand side it is: where its task stands
    /// while it waits.
    pub statement: Site,
    pub kind: SiteKind,
    /// The loops enclosing it inside its own function body, outermost
    /// first.
    pub loops: Vec<Site>,
}

/// What an action does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SiteKind {
    Call,
    Perform,
    Sleep,
    Join,
    JoinMany,
    Yield,
    Spawn,
    Cancel,
}

impl std::fmt::Display for SiteKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Call => "call",
            Self::Perform => "perform",
            Self::Sleep => "sleep",
            Self::Join => "join",
            Self::JoinMany => "join_many",
            Self::Yield => "yield",
            Self::Spawn => "spawn",
            Self::Cancel => "cancel",
        })
    }
}

/// Why a graph has no view of an entry.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ViewError {
    #[error("the graph has no unit {unit:?}")]
    NoSuchUnit { unit: Unit },
    #[error("the view's catalog has no library function {function}")]
    MissingFunction { function: FunctionId },
    /// The graph was derived from another document.
    #[error("the graph does not describe the document at {site:?}")]
    Mismatch { site: Site },
}

/// The view of `entry` in `document`, read from the `graph` the checker
/// derived from it. `functions` must be the catalog used to derive the graph:
/// document `use` lines name library identities but carry no signatures.
/// A decoded [`lash_kernel_doc::FunctionDeclarations`] is sufficient.
///
/// # Errors
///
/// The graph has no such unit or was not derived from `document`, or the
/// catalog lacks a called library function.
pub fn derive(
    document: &Document,
    graph: &Graph,
    entry: &Unit,
    functions: &dyn FunctionCatalog,
) -> Result<WorkflowView, ViewError> {
    let unit = graph.unit(entry).ok_or_else(|| ViewError::NoSuchUnit {
        unit: entry.clone(),
    })?;
    let body = match entry {
        Unit::Main => Node::Block(&document.main),
        Unit::Function(name) => match document.functions.get(name) {
            Some(function) => Node::Block(&function.body),
            None => return Err(mismatch(graph.node(unit.body))),
        },
        Unit::Library(_) => return Err(mismatch(graph.node(unit.body))),
    };
    let expected = match entry {
        Unit::Function(name) => document.entries.get(name).map(|entry| entry.result.clone()),
        Unit::Main | Unit::Library(_) => None,
    };
    let mut walk = Walk {
        document,
        graph,
        functions,
        entry: Callable {
            site: Site::new(entry.clone(), Vec::new()),
            expected: expected.unwrap_or(Type::Any),
        },
        view: WorkflowView {
            statements: Vec::new(),
            scopes: Vec::new(),
            dataflow: Dataflow {
                parameters: unit.params.iter().map(|id| id.index()).collect(),
                ..Dataflow::default()
            },
            execution_sites: graph.execution_sites().iter().map(execution_site).collect(),
        },
        uses: BTreeMap::new(),
    };
    let at = Context {
        block: walk.entry.site.clone(),
        statement: None,
        depth: 0,
        callable: walk.entry.clone(),
        assigns: None,
        opens: Some(ScopeKind::Entry),
    };
    walk.node(unit.body, body, &at)?;
    let Walk {
        mut view, mut uses, ..
    } = walk;
    view.dataflow.bindings = graph
        .bindings()
        .iter()
        .filter(|binding| graph.node(binding.declared_at).site.unit == *entry)
        .map(|binding| {
            let declaration = graph.node(binding.declared_at);
            BindingView {
                id: binding.id.index(),
                name: binding.name.clone(),
                kind: match binding.kind {
                    lash_kernel_check::BindingKind::Param => BindingKind::Param,
                    lash_kernel_check::BindingKind::Let => BindingKind::Let,
                    lash_kernel_check::BindingKind::For => BindingKind::For,
                    lash_kernel_check::BindingKind::Catch => BindingKind::Catch,
                },
                site: declaration.site.clone(),
                block: graph.node(graph.scope(binding.scope).block).site.clone(),
                ty: binding.facet.clone(),
                assigned: binding.assigned,
                captured: binding.captured,
                value_site: match declaration.kind {
                    NodeKind::Stmt(StmtNode::Let { value, .. }) => {
                        Some(graph.node(value).site.clone())
                    }
                    _ => None,
                },
                uses: uses.remove(&binding.id).unwrap_or_default(),
            }
        })
        .collect();
    Ok(view)
}

fn execution_site(site: &lash_kernel_check::ExecutionSite) -> ExecutionSiteView {
    use lash_kernel_check::SiteKind as Graph;
    ExecutionSiteView {
        site: site.site.clone(),
        statement: site.statement.clone(),
        kind: match site.kind {
            Graph::Call => SiteKind::Call,
            Graph::Perform => SiteKind::Perform,
            Graph::Sleep => SiteKind::Sleep,
            Graph::Join => SiteKind::Join,
            Graph::JoinMany => SiteKind::JoinMany,
            Graph::Yield => SiteKind::Yield,
            Graph::Spawn => SiteKind::Spawn,
            Graph::Cancel => SiteKind::Cancel,
        },
        loops: site.loops.clone(),
    }
}

fn mismatch(node: &GraphNode) -> ViewError {
    ViewError::Mismatch {
        site: node.site.clone(),
    }
}

/// What a `return` ends.
#[derive(Clone)]
struct Callable {
    site: Site,
    expected: Type,
}

/// Where the walk stands.
struct Context {
    block: Site,
    statement: Option<Site>,
    depth: usize,
    callable: Callable,
    /// What the enclosing statement writes, when it is an assignment.
    assigns: Option<Place>,
    /// What the node opens when it is a block.
    opens: Option<ScopeKind>,
}

struct Walk<'a> {
    document: &'a Document,
    graph: &'a Graph,
    functions: &'a dyn FunctionCatalog,
    entry: Callable,
    view: WorkflowView,
    uses: BTreeMap<BindingId, Vec<Site>>,
}

impl<'a> Walk<'a> {
    fn node(&mut self, id: NodeId, written: Node<'a>, at: &Context) -> Result<(), ViewError> {
        let graph = self.graph;
        let node = graph.node(id);
        for binding in direct_references(&node.kind) {
            self.uses
                .entry(binding)
                .or_default()
                .push(node.site.clone());
        }
        let mut inner = Context {
            block: at.block.clone(),
            statement: at.statement.clone(),
            depth: at.depth,
            callable: at.callable.clone(),
            assigns: None,
            opens: None,
        };
        // The blocks this node opens, each with what it is the body of.
        let mut opens: Vec<(NodeId, ScopeKind)> = Vec::new();
        match (&node.kind, written) {
            (NodeKind::Block(_), Node::Block(_)) => {
                let kind = at.opens.ok_or_else(|| mismatch(node))?;
                self.view.scopes.push(ScopeView {
                    block: node.site.clone(),
                    kind,
                });
                inner.block = node.site.clone();
            }
            (NodeKind::Stmt(statement), Node::Stmt(written)) => {
                self.statement(node, statement, written, at, &mut opens);
                inner.statement = Some(node.site.clone());
                inner.depth += 1;
                inner.assigns = match written {
                    Stmt::Assign { place, .. } => Some(place.clone()),
                    _ => None,
                };
            }
            (NodeKind::Action(action), Node::Action(written)) => {
                self.action(node, action, written, at)?;
            }
            (NodeKind::Expr(expression), Node::Expr(written)) => {
                self.view.dataflow.expressions.push(ExpressionView {
                    site: node.site.clone(),
                    statement: at.statement.clone().ok_or_else(|| mismatch(node))?,
                    block: at.block.clone(),
                    expression: written.clone(),
                    ty: node.facet.clone().unwrap_or(Type::Any),
                    references: references(graph, id),
                    available: available(graph, node),
                });
                match expression {
                    ExprNode::Closure { body, .. } => {
                        opens.push((*body, ScopeKind::Closure));
                        inner.callable = Callable {
                            site: node.site.clone(),
                            expected: match &node.facet {
                                Some(Type::Function(signature)) => signature.result.clone(),
                                _ => Type::Any,
                            },
                        };
                    }
                    ExprNode::Literal(_)
                    | ExprNode::Variable(_)
                    | ExprNode::Tuple(_)
                    | ExprNode::List(_)
                    | ExprNode::Map(_)
                    | ExprNode::Set(_)
                    | ExprNode::Record(_)
                    | ExprNode::Member(_)
                    | ExprNode::Call { .. }
                    | ExprNode::Clock
                    | ExprNode::Random
                    | ExprNode::Read { .. } => {}
                }
            }
            (
                NodeKind::Block(_) | NodeKind::Stmt(_) | NodeKind::Action(_) | NodeKind::Expr(_),
                _,
            ) => {
                return Err(mismatch(node));
            }
        }
        let children = node.children();
        let written = written.children();
        if children.len() != written.len() {
            return Err(mismatch(node));
        }
        for (child, written) in children.into_iter().zip(written) {
            inner.opens = opens
                .iter()
                .find(|(block, _)| *block == child)
                .map(|(_, kind)| *kind);
            self.node(child, written, &inner)?;
        }
        Ok(())
    }

    fn statement(
        &mut self,
        node: &GraphNode,
        statement: &StmtNode,
        written: &Stmt,
        at: &Context,
        opens: &mut Vec<(NodeId, ScopeKind)>,
    ) {
        let graph = self.graph;
        self.view.statements.push(StatementView {
            site: node.site.clone(),
            statement: written.clone(),
            block: at.block.clone(),
            depth: at.depth,
            action: node
                .children()
                .into_iter()
                .map(|child| graph.node(child))
                .find(|child| matches!(child.kind, NodeKind::Action(_)))
                .map(|child| child.site.clone()),
        });
        match statement {
            StmtNode::If {
                then_block,
                else_block,
                ..
            } => opens.extend([
                (*then_block, ScopeKind::Then),
                (*else_block, ScopeKind::Else),
            ]),
            StmtNode::For { body, .. } => opens.push((*body, ScopeKind::For)),
            StmtNode::While { body, .. } => opens.push((*body, ScopeKind::While)),
            StmtNode::Try {
                body,
                catch,
                finally,
            } => {
                opens.push((*body, ScopeKind::Try));
                opens.extend(catch.as_ref().map(|catch| (catch.body, ScopeKind::Catch)));
                opens.extend(finally.map(|finally| (finally, ScopeKind::Finally)));
            }
            StmtNode::Return { value } | StmtNode::Finish { value } => {
                let callable = if matches!(statement, StmtNode::Finish { .. }) {
                    &self.entry
                } else {
                    &at.callable
                };
                let value = graph.node(*value);
                self.view.dataflow.returns.push(ReturnView {
                    callable: callable.site.clone(),
                    expected_type: callable.expected.clone(),
                    statement: node.site.clone(),
                    expression_site: value.site.clone(),
                    ty: value.facet.clone().unwrap_or(Type::Any),
                });
            }
            StmtNode::Assign { value, .. } => {
                if let Stmt::Assign { place, .. } = written {
                    self.view.dataflow.assignments.push(AssignmentView {
                        statement: node.site.clone(),
                        value_site: graph.node(*value).site.clone(),
                        destination: place.clone(),
                    });
                }
            }
            StmtNode::Let { .. }
            | StmtNode::Remove { .. }
            | StmtNode::Do { .. }
            | StmtNode::Break { .. }
            | StmtNode::Continue { .. }
            | StmtNode::Throw { .. }
            | StmtNode::Print { .. }
            | StmtNode::Fail { .. } => {}
        }
    }

    fn action(
        &mut self,
        node: &GraphNode,
        derived: &ActionNode,
        written: &Action,
        at: &Context,
    ) -> Result<(), ViewError> {
        let statement = at.statement.clone().ok_or_else(|| mismatch(node))?;
        let signature = match written {
            Action::Perform { effect, .. } => self.document.manifest.effects.get(effect),
            _ => None,
        };
        let (callee, params) = match written {
            Action::Call { callee, .. } | Action::Spawn { callee, .. } => match callee {
                Callee::Library(function) => {
                    let definition =
                        self.functions
                            .definition(function)
                            .ok_or(ViewError::MissingFunction {
                                function: *function,
                            })?;
                    (
                        Some(CalleeView::Library(definition.name.clone())),
                        definition.signature.params.clone(),
                    )
                }
                Callee::Declared(name) => {
                    let function = self
                        .document
                        .functions
                        .get(name)
                        .ok_or_else(|| mismatch(node))?;
                    let params = self.document.entries.get(name).map_or_else(
                        || {
                            function
                                .params
                                .iter()
                                .map(|name| Param {
                                    name: name.clone(),
                                    ty: Type::Any,
                                    optional: false,
                                })
                                .collect()
                        },
                        |signature| signature.params.clone(),
                    );
                    (Some(CalleeView::Declared(name.clone())), params)
                }
                Callee::Value(_) => (None, Vec::new()),
            },
            _ => (
                None,
                signature.map_or_else(Vec::new, |signature| signature.params.clone()),
            ),
        };
        let args = written_atoms(written)
            .into_iter()
            .zip(atoms(derived))
            .enumerate()
            .map(|(index, (atom, derived))| {
                let param = params.get(index);
                ArgumentView {
                    name: param.map(|param| param.name.clone()),
                    ty: param.map_or(Type::Any, |param| param.ty.clone()),
                    atom: atom.clone(),
                    binding: match derived {
                        AtomNode::Variable(Reference::Binding(id)) => Some(id.index()),
                        AtomNode::Variable(Reference::Session(_)) | AtomNode::Literal(_) => None,
                    },
                }
            })
            .collect();
        let result_type = match derived {
            ActionNode::Perform {
                result: Type::Any, ..
            } => signature.map_or(Type::Any, |signature| signature.result.clone()),
            _ => node.facet.clone().unwrap_or(Type::Any),
        };
        self.view.dataflow.actions.push(ActionView {
            site: node.site.clone(),
            statement,
            block: at.block.clone(),
            destination: at.assigns.clone(),
            callee,
            args,
            result_type,
            available: available(self.graph, node),
        });
        Ok(())
    }
}

/// The bindings a node names itself, not through its children. The target
/// of an assignment is a write, not a read.
fn direct_references(kind: &NodeKind) -> Vec<BindingId> {
    let references: Vec<&Reference> = match kind {
        NodeKind::Expr(expression) => match expression {
            ExprNode::Variable(reference) => vec![reference],
            ExprNode::Closure { captures, .. } => captures.iter().collect(),
            ExprNode::Literal(_)
            | ExprNode::Tuple(_)
            | ExprNode::List(_)
            | ExprNode::Map(_)
            | ExprNode::Set(_)
            | ExprNode::Record(_)
            | ExprNode::Member(_)
            | ExprNode::Call { .. }
            | ExprNode::Clock
            | ExprNode::Random
            | ExprNode::Read { .. } => Vec::new(),
        },
        NodeKind::Action(action) => {
            let callee = match action {
                ActionNode::Call { callee, .. } | ActionNode::Spawn { callee, .. } => {
                    match callee {
                        CalleeNode::Value(reference) => Some(reference),
                        CalleeNode::Declared(_) | CalleeNode::Library(_) => None,
                    }
                }
                ActionNode::Perform { .. }
                | ActionNode::Sleep { .. }
                | ActionNode::Join { .. }
                | ActionNode::JoinMany { .. }
                | ActionNode::Yield
                | ActionNode::Cancel { .. } => None,
            };
            callee
                .into_iter()
                .chain(atoms(action).into_iter().filter_map(|atom| match atom {
                    AtomNode::Variable(reference) => Some(reference),
                    AtomNode::Literal(_) => None,
                }))
                .collect()
        }
        NodeKind::Stmt(_) | NodeKind::Block(_) => Vec::new(),
    };
    references
        .into_iter()
        .filter_map(|reference| match reference {
            Reference::Binding(id) => Some(*id),
            Reference::Session(_) => None,
        })
        .collect()
}

fn atoms(action: &ActionNode) -> Vec<&AtomNode> {
    match action {
        ActionNode::Call { args, .. }
        | ActionNode::Spawn { args, .. }
        | ActionNode::Perform { args, .. } => args.iter().collect(),
        ActionNode::Sleep { duration } => vec![duration],
        ActionNode::Join { task } | ActionNode::Cancel { task } => vec![task],
        ActionNode::JoinMany { tasks, .. } => vec![tasks],
        ActionNode::Yield => vec![],
    }
}

fn written_atoms(action: &Action) -> Vec<&Atom> {
    match action {
        Action::Call { args, .. } | Action::Spawn { args, .. } | Action::Perform { args, .. } => {
            args.iter().collect()
        }
        Action::Sleep { duration } => vec![duration],
        Action::Join { task } | Action::Cancel { task } => vec![task],
        Action::JoinMany { tasks, .. } => vec![tasks],
        Action::Yield => vec![],
    }
}

/// The bindings a node and its children read, in id order. A closure's body
/// reads for the closure through its captures.
fn references(graph: &Graph, id: NodeId) -> Vec<usize> {
    fn collect(graph: &Graph, id: NodeId, out: &mut std::collections::BTreeSet<usize>) {
        let node = graph.node(id);
        out.extend(
            direct_references(&node.kind)
                .into_iter()
                .map(BindingId::index),
        );
        if !matches!(node.kind, NodeKind::Expr(ExprNode::Closure { .. })) {
            for child in node.children() {
                collect(graph, child, out);
            }
        }
    }
    let mut out = std::collections::BTreeSet::new();
    collect(graph, id, &mut out);
    out.into_iter().collect()
}

/// The binding each name resolves to at `node`, as lexical lookup would: a
/// binding declared at or after the node, or whose `let` holds it, is not
/// yet visible, and an inner binding shadows an outer one of its name.
fn available(graph: &Graph, node: &GraphNode) -> Vec<usize> {
    let mut scope = Some(node.scope);
    let mut names = BTreeMap::new();
    while let Some(id) = scope {
        let current = graph.scope(id);
        for binding_id in current.bindings.iter().rev() {
            let binding = graph.binding(*binding_id);
            let declaration = graph.node(binding.declared_at);
            if binding.kind == lash_kernel_check::BindingKind::Let
                && (declaration.id.index() >= node.id.index()
                    || (declaration.site.unit == node.site.unit
                        && node.site.path.starts_with(&declaration.site.path)))
            {
                continue;
            }
            names
                .entry(binding.name.clone())
                .or_insert(binding.id.index());
        }
        scope = current.parent;
    }
    names.into_values().collect()
}
