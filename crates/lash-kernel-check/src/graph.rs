//! The host-facing graph view: the total typed read model of a document
//! (`K-DOC-001`).
//!
//! Every construct of the document is a [`GraphNode`]: a block, a statement,
//! an action or an expression, each a typed variant whose slots name their
//! child nodes. Nothing is opaque and nothing is source text. Everything
//! here is derived from the document and never stored as authority: the
//! view is rebuilt on every change.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{
    EffectName, FunctionId, JoinMode, Literal, Name, Signature, Site, Type, Unit,
};

macro_rules! dense_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub(crate) u32);

        impl $name {
            /// The id's position in the view's table of its kind.
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

dense_id!(
    /// A node of one derived view (`K-SITE-005`): its position in a
    /// pre-order walk of the units, `main` first and then the declared
    /// functions by name. It names a node within one derivation of one
    /// document; a [`Site`] is the address that is saved.
    NodeId
);
dense_id!(
    /// A variable declaration of one derived view, in declaration order.
    BindingId
);
dense_id!(
    /// A scope of one derived view, in the order the scopes open.
    ScopeId
);

/// The derived view of one document, or of one library function's body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Graph {
    pub(crate) units: Vec<UnitView>,
    pub(crate) nodes: Vec<GraphNode>,
    pub(crate) scopes: Vec<Scope>,
    pub(crate) bindings: Vec<Binding>,
    pub(crate) edges: Vec<Edge>,
    pub(crate) sites: BTreeMap<Site, NodeId>,
    pub(crate) execution_sites: Vec<ExecutionSite>,
    pub(crate) session_reads: BTreeMap<Name, Vec<NodeId>>,
}

impl Graph {
    pub(crate) fn empty() -> Self {
        Self {
            units: Vec::new(),
            nodes: Vec::new(),
            scopes: Vec::new(),
            bindings: Vec::new(),
            edges: Vec::new(),
            sites: BTreeMap::new(),
            execution_sites: Vec::new(),
            session_reads: BTreeMap::new(),
        }
    }

    /// The units: `main` first, then the declared functions by name. The
    /// view of a library body has the one unit.
    pub fn units(&self) -> &[UnitView] {
        &self.units
    }

    pub fn unit(&self, unit: &Unit) -> Option<&UnitView> {
        self.units.iter().find(|view| &view.unit == unit)
    }

    /// Every node, indexed by [`NodeId`].
    pub fn nodes(&self) -> &[GraphNode] {
        &self.nodes
    }

    pub fn node(&self, id: NodeId) -> &GraphNode {
        &self.nodes[id.index()]
    }

    /// The node a site addresses.
    pub fn node_at(&self, site: &Site) -> Option<&GraphNode> {
        self.sites.get(site).map(|id| self.node(*id))
    }

    /// Every scope, indexed by [`ScopeId`].
    pub fn scopes(&self) -> &[Scope] {
        &self.scopes
    }

    pub fn scope(&self, id: ScopeId) -> &Scope {
        &self.scopes[id.index()]
    }

    /// Every variable declaration, indexed by [`BindingId`].
    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    pub fn binding(&self, id: BindingId) -> &Binding {
        &self.bindings[id.index()]
    }

    /// The control and call edges, ordered by source node.
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    pub fn edges_from(&self, node: NodeId) -> impl Iterator<Item = &Edge> {
        self.edges.iter().filter(move |edge| edge.from == node)
    }

    /// Every action of the view, in node order: the coordinates of waits and
    /// spawns (`K-SITE-002`).
    pub fn execution_sites(&self) -> &[ExecutionSite] {
        &self.execution_sites
    }

    /// The site of every statement, in node order: where a task may stand
    /// (`K-SITE-001`).
    pub fn statement_sites(&self) -> impl Iterator<Item = &Site> {
        self.nodes
            .iter()
            .filter(|node| matches!(node.kind, NodeKind::Stmt(_)))
            .map(|node| &node.site)
    }

    /// The site of every `for` and `while`, in node order (`K-SITE-003`).
    pub fn loop_sites(&self) -> impl Iterator<Item = &Site> {
        self.nodes
            .iter()
            .filter(|node| {
                matches!(
                    node.kind,
                    NodeKind::Stmt(StmtNode::For { .. } | StmtNode::While { .. })
                )
            })
            .map(|node| &node.site)
    }

    /// The variables `main` reads or assigns that it has not declared at
    /// that point, with the nodes that name them: the bindings it needs from
    /// its session (`K-SES-001`).
    pub fn session_reads(&self) -> &BTreeMap<Name, Vec<NodeId>> {
        &self.session_reads
    }
}

/// One unit of code: `main`, a declared function or a library body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitView {
    pub unit: Unit,
    /// The unit's body block.
    pub body: NodeId,
    pub params: Vec<BindingId>,
    /// The signature the unit is started under, when the document lists it
    /// as an entry; a library body's own signature.
    pub signature: Option<Signature>,
    pub effects: EffectSet,
}

/// What a unit may perform (`K-FN-001`): derived, never declared.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectSet {
    /// The effects performed by the unit's own code, the closures it
    /// writes, and every declared function it calls, spawns or references,
    /// transitively.
    pub effects: BTreeSet<EffectName>,
    /// The unit, or a function it reaches, calls a function held in a
    /// variable or a library function with a kernel-code body. What such a
    /// call performs depends on the value passed, so `effects` bounds only
    /// the code named above.
    pub through_values: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphNode {
    pub id: NodeId,
    pub site: Site,
    pub parent: Option<NodeId>,
    /// The scope the node is evaluated in. A block's is the scope it opens.
    pub scope: ScopeId,
    pub kind: NodeKind,
    /// The type facet of an expression's value or an action's result, as far
    /// as the document alone tells. `None` for a block and a statement.
    pub facet: Option<Type>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// The statements, in order.
    Block(Vec<NodeId>),
    Stmt(StmtNode),
    Action(ActionNode),
    Expr(ExprNode),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StmtNode {
    Let {
        binding: BindingId,
        value: NodeId,
    },
    Assign {
        place: PlaceNode,
        value: NodeId,
    },
    Remove {
        member: MemberNode,
    },
    Do {
        action: NodeId,
    },
    If {
        condition: NodeId,
        then_block: NodeId,
        else_block: NodeId,
    },
    For {
        binding: BindingId,
        iterable: NodeId,
        body: NodeId,
    },
    While {
        condition: NodeId,
        body: NodeId,
    },
    /// `target` is the loop it ends.
    Break {
        target: NodeId,
    },
    /// `target` is the loop it continues.
    Continue {
        target: NodeId,
    },
    Return {
        value: NodeId,
    },
    Throw {
        value: NodeId,
    },
    Print {
        value: NodeId,
    },
    Finish {
        value: NodeId,
    },
    Fail {
        value: NodeId,
    },
    Try {
        body: NodeId,
        catch: Option<CatchNode>,
        finally: Option<NodeId>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatchNode {
    pub binding: BindingId,
    pub body: NodeId,
}

/// What a name in the code refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reference {
    /// A variable the unit declares.
    Binding(BindingId),
    /// A variable `main` has not declared at this point: a session binding
    /// (`K-SES-001`).
    Session(Name),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaceNode {
    Variable(Reference),
    Member(MemberNode),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberNode {
    Field { target: NodeId, field: String },
    Index { target: NodeId, index: NodeId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionNode {
    Call {
        callee: CalleeNode,
        args: Vec<AtomNode>,
    },
    Perform {
        effect: EffectName,
        args: Vec<AtomNode>,
        result: Type,
    },
    Sleep {
        duration: AtomNode,
    },
    Join {
        task: AtomNode,
    },
    JoinMany {
        mode: JoinMode,
        tasks: AtomNode,
    },
    Yield,
    Spawn {
        callee: CalleeNode,
        args: Vec<AtomNode>,
    },
    Cancel {
        task: AtomNode,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CalleeNode {
    Declared(Name),
    Value(Reference),
    Library(FunctionId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AtomNode {
    Variable(Reference),
    Literal(Literal),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExprNode {
    Literal(Literal),
    Variable(Reference),
    Tuple(Vec<NodeId>),
    List(Vec<NodeId>),
    Map(Vec<MapEntryNode>),
    Set(Vec<NodeId>),
    Record(Vec<RecordEntryNode>),
    Member(MemberNode),
    Closure {
        params: Vec<BindingId>,
        body: NodeId,
        /// The variables of enclosing scopes the closure shares, in the
        /// order its body first names them (`K-CLO-001`).
        captures: Vec<Reference>,
    },
    Call {
        function: FunctionId,
        args: Vec<NodeId>,
    },
    Clock,
    Random,
    Read {
        handle: NodeId,
        request: NodeId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapEntryNode {
    pub key: NodeId,
    pub value: NodeId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordEntryNode {
    pub field: String,
    pub value: NodeId,
}

impl MemberNode {
    fn push_children(&self, out: &mut Vec<NodeId>) {
        match self {
            Self::Field { target, .. } => out.push(*target),
            Self::Index { target, index } => out.extend([*target, *index]),
        }
    }
}

impl GraphNode {
    /// The node's children, one per slot, in the order a [`Site`] counts
    /// them (`K-ID-004`): the child at position `i` has this node's site
    /// with `i` appended.
    pub fn children(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        match &self.kind {
            NodeKind::Block(statements) => out.extend(statements),
            NodeKind::Action(_) => {}
            NodeKind::Stmt(stmt) => match stmt {
                StmtNode::Let { value, .. } => out.push(*value),
                StmtNode::Assign { place, value } => {
                    if let PlaceNode::Member(member) = place {
                        member.push_children(&mut out);
                    }
                    out.push(*value);
                }
                StmtNode::Remove { member } => member.push_children(&mut out),
                StmtNode::Do { action } => out.push(*action),
                StmtNode::If {
                    condition,
                    then_block,
                    else_block,
                } => out.extend([*condition, *then_block, *else_block]),
                StmtNode::For { iterable, body, .. } => out.extend([*iterable, *body]),
                StmtNode::While { condition, body } => out.extend([*condition, *body]),
                StmtNode::Break { .. } | StmtNode::Continue { .. } => {}
                StmtNode::Return { value }
                | StmtNode::Throw { value }
                | StmtNode::Print { value }
                | StmtNode::Finish { value }
                | StmtNode::Fail { value } => out.push(*value),
                StmtNode::Try {
                    body,
                    catch,
                    finally,
                } => {
                    out.push(*body);
                    out.extend(catch.as_ref().map(|catch| catch.body));
                    out.extend(finally);
                }
            },
            NodeKind::Expr(expr) => match expr {
                ExprNode::Literal(_)
                | ExprNode::Variable(_)
                | ExprNode::Clock
                | ExprNode::Random => {}
                ExprNode::Tuple(items) | ExprNode::List(items) | ExprNode::Set(items) => {
                    out.extend(items);
                }
                ExprNode::Map(entries) => {
                    out.extend(entries.iter().flat_map(|entry| [entry.key, entry.value]));
                }
                ExprNode::Record(entries) => out.extend(entries.iter().map(|entry| entry.value)),
                ExprNode::Member(member) => member.push_children(&mut out),
                ExprNode::Closure { body, .. } => out.push(*body),
                ExprNode::Call { args, .. } => out.extend(args),
                ExprNode::Read { handle, request } => out.extend([*handle, *request]),
            },
        }
        out
    }
}

/// A scope: one block (`K-FORM-003`). A function's parameters, a `for`
/// binding and a `catch` binding are declared in the scope of the body they
/// belong to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub id: ScopeId,
    /// The enclosing scope. A closure body's is the scope that writes the
    /// closure; a unit body has none.
    pub parent: Option<ScopeId>,
    pub block: NodeId,
    /// The variables declared here, in declaration order. A later one of the
    /// same name shadows an earlier one from its declaration on.
    pub bindings: Vec<BindingId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BindingKind {
    Param,
    Let,
    For,
    Catch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub id: BindingId,
    pub name: Name,
    pub kind: BindingKind,
    /// The node that declares it: the `let`, the `for`, the `try` of its
    /// `catch`, the closure expression, or a unit's body for its parameters.
    pub declared_at: NodeId,
    pub scope: ScopeId,
    /// What the variable holds, as far as the document alone tells: the
    /// facet of its `let` value when nothing assigns it, with the contents
    /// of mutable objects left open; otherwise `Any`.
    pub facet: Type,
    /// Some statement assigns the variable after its declaration.
    pub assigned: bool,
    /// A closure shares the variable (`K-CLO-001`).
    pub captured: bool,
}

/// Where an edge leads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Target {
    Node(NodeId),
    /// Out of the function body: to the caller, or the end of the task.
    Leave,
    /// The end of the run.
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EdgeKind {
    /// To the statement that runs next when this one completes. A loop
    /// body's last statement leads back to its loop; a loop leads on when it
    /// ends.
    Next,
    /// From an `if` to its first statement when the condition holds.
    Then,
    /// From an `if` to its first statement when the condition does not.
    Else,
    /// From a loop or a `try` into its body.
    Body,
    Break,
    Continue,
    Return,
    /// From a `throw` to the handler that takes it. Raises of other
    /// statements are not drawn: any statement may raise.
    Throw,
    /// From a `try` to its `catch` body.
    Catch,
    /// From a `try` to its `finally` block.
    Finally,
    /// From a `finish` or a `fail`.
    End,
    /// From a call action to the declared function's body.
    Call,
    /// From a spawn action to the declared function's body.
    Spawn,
    /// From a node that holds a function-reference literal to that
    /// function's body.
    Reference,
}

/// One derived edge. A `break`, `continue`, `return` or `throw` that leaves
/// a `try` with a `finally` leads to the `finally` block's first statement;
/// the departure then resumes from the `finally` block node, as an edge of
/// the same kind (`K-FORM-017`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Edge {
    pub from: NodeId,
    pub to: Target,
    pub kind: EdgeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

impl SiteKind {
    /// Whether the action is a wait (`K-TASK-004`).
    pub fn waits(self) -> bool {
        matches!(
            self,
            Self::Perform | Self::Sleep | Self::Join | Self::JoinMany | Self::Yield
        )
    }
}

/// One action and the coordinates it runs under (`K-SITE-002`,
/// `K-SITE-004`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionSite {
    pub node: NodeId,
    /// The action's site: what an effect's or a spawn's identity carries.
    pub site: Site,
    /// The site of the statement the action is the right-hand side of: where
    /// its task stands while it waits.
    pub statement: Site,
    pub kind: SiteKind,
    /// The loops that enclose the action inside its own function body,
    /// outermost first. A closure body is a function body.
    pub loops: Vec<Site>,
}
