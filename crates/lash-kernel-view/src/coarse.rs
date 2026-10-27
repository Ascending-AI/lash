//! Canonical, revision-local coarse membership. No dialect or runtime executes here.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_check::{
    EdgeKind, ExprNode, Graph, MemberNode, NodeKind, PlaceNode, Reference, StmtNode, Target,
};
use lash_kernel_doc::{
    Document, Expr, FunctionDeclarations, SourceMap, SourceMapRefusal, Span, Stmt,
};

use lash_kernel_value::sites::Site;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::comparison::ComparisonView;
use crate::{ScopeKind, StatementView, WorkflowView};

/// A lexical callable is projected once, independently of its invocations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CallableView {
    pub site: Site,
    pub block: Site,
}

/// Inclusive, one-based lines in an explicitly named printed lens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LineRange {
    pub first: usize,
    pub last: usize,
}

/// A control retains empty arms and the distinction between body entries and
/// header actions. Header actions live under `header`, not before the control.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlArm {
    pub role: ScopeKind,
    pub block: Site,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HeaderRole {
    Condition,
    Iterable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlHeader {
    pub role: HeaderRole,
    pub expression: Site,
    pub actions: Vec<Site>,
    /// The comparison a condition decides, read through a variable the
    /// lowering bound to it in the statement right before the control and
    /// that nothing else reads or reassigns. Absent when the condition is
    /// not one comparison of parameters, literals, action results and
    /// their record fields; never a guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<ComparisonView>,
}

/// An expression alternative that guards an action's evaluation. These are
/// ancestry, not scheduling edges: static issue order never implies both arms run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LazyArm {
    pub expression: Site,
    pub arm: usize,
}

/// Kind-specific workflow data. The tag owns all variant payload fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowNodeBody {
    Action {
        /// Each statement has one carrier; secondary actions have no members.
        members: Vec<Site>,
        within: Option<Site>,
        lazy: Vec<LazyArm>,
    },
    Control {
        members: Vec<Site>,
        condition_ordinal: Option<u32>,
        header: Option<Box<ControlHeader>>,
        arms: Vec<ControlArm>,
    },
    Code {
        members: NonEmptySites,
    },
}

/// The statement coordinates in a nonempty code run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Site>")]
pub struct NonEmptySites(Vec<Site>);
impl NonEmptySites {
    pub fn new(first: Site) -> Self {
        Self(vec![first])
    }

    pub fn as_slice(&self) -> &[Site] {
        &self.0
    }
}
impl TryFrom<Vec<Site>> for NonEmptySites {
    type Error = &'static str;
    fn try_from(members: Vec<Site>) -> Result<Self, Self::Error> {
        if members.is_empty() {
            Err("code node needs at least one member")
        } else {
            Ok(Self(members))
        }
    }
}

impl JsonSchema for NonEmptySites {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "NonEmptySites".into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = Vec::<Site>::json_schema(generator);
        schema.insert("minItems".into(), 1.into());
        schema
    }
}
impl WorkflowNodeBody {
    pub fn members(&self) -> &[Site] {
        match self {
            Self::Action { members, .. } | Self::Control { members, .. } => members,
            Self::Code { members } => members.as_slice(),
        }
    }
}
impl WorkflowNode {
    pub fn members(&self) -> &[Site] {
        self.body.members()
    }
}

/// Identity is (view.document, body kind, block, site, last member). Ranges are
/// display metadata. Supporting statements and dataflow remain addressable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowNode {
    pub body: WorkflowNodeBody,
    pub site: Site,
    pub block: Site,
    pub callable: Site,
    /// The owning statement for actions/controls; first member for code.
    pub statement: Site,
    /// The node's authoritative count coordinate in RanCounts.
    pub ordinal: u32,
    pub bindings: Vec<usize>,
    pub reads: Vec<usize>,
    pub writes: Vec<usize>,
    pub readable: Vec<LineRange>,
    /// Exact document action and statement sites folded into this authored
    /// node. Execution counts and replay paths retain these coordinates.
    pub folded: Vec<Site>,
    pub departures: Vec<Site>,
    pub falls_through: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowTarget {
    Site(Site),
    Leave,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowKind {
    Next,
    Then,
    Else,
    Body,
    Break,
    Continue,
    Return,
    Throw,
    Catch,
    Finally,
    End,
    Call,
    Spawn,
    Reference,
}

/// Canonical routes at exact member Sites, including finally-unwind routes.
/// Hosts must not invent a fallthrough between coarse groups.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowEdge {
    pub from: Site,
    pub to: FlowTarget,
    pub kind: FlowKind,
}

/// The requested readable source, or why no source is available.
/// Projection remains available when printing is refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReadableLens {
    Unrequested,
    Available {
        name: String,
        source: String,
    },
    Refused {
        name: String,
        cause: ReadableRefusal,
    },
}

/// Typed printer refusal. The projection remains available without source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReadableRefusal {
    Unprintable { site: Site },
    Bytes { limit: usize },
    Depth { limit: usize },
    Encoding { message: String },
    Invalid { message: String },
}

impl WorkflowView {
    /// Resolve an execution or replay coordinate to its authored node.
    /// Lowering-only sites resolve to their consumer through `folded`;
    /// authored sibling actions retain their distinct coordinates.
    pub fn node_at(&self, site: &Site) -> Option<&WorkflowNode> {
        self.nodes
            .iter()
            .find(|node| node.site == *site || node.folded.contains(site))
            .or_else(|| self.nodes.iter().find(|node| node.members().contains(site)))
    }

    pub fn action_node(
        &self,
        site: &lash_kernel_value::sites::PositionLabel,
    ) -> Option<&WorkflowNode> {
        self.nodes.iter().find(|node| {
            matches!(node.body, WorkflowNodeBody::Action { .. })
                && (node.site == *site || node.folded.iter().any(|folded| *folded == *site))
        })
    }

    /// Record a printer refusal and discard ranges from any previous lens.
    pub fn refuse_readable(&mut self, name: String, cause: ReadableRefusal) {
        for node in &mut self.nodes {
            node.readable.clear();
        }
        for statement in &mut self.statements {
            statement.readable.clear();
        }
        self.readable = ReadableLens::Refused { name, cause };
    }

    /// Attach the ranges a readable printer actually produced, validated for
    /// this exact document and source. No source parsing or span guessing.
    pub fn readable_ranges(
        &mut self,
        document: &Document,
        name: String,
        source: &str,
        map: &SourceMap,
    ) -> Result<(), SourceMapRefusal> {
        if self.document != document.identity() {
            return Err(SourceMapRefusal::Document);
        }
        map.validate(document, source)?;
        let ranges: BTreeMap<_, _> = map
            .sites
            .iter()
            .map(|entry| (&entry.site, entry.location.ranges()))
            .collect();
        for node in &mut self.nodes {
            let sites = if matches!(node.body, WorkflowNodeBody::Code { .. }) {
                node.members().to_vec()
            } else {
                vec![node.site.clone()]
            };
            node.readable = sites
                .iter()
                .flat_map(|site| {
                    ranges
                        .get(site)
                        .into_iter()
                        .flat_map(|ranges| ranges.iter())
                        .map(|range| lines(source, *range))
                })
                .collect();
        }
        for statement in &mut self.statements {
            statement.readable = ranges
                .get(&statement.site)
                .into_iter()
                .flat_map(|ranges| ranges.iter())
                .map(|range| lines(source, *range))
                .collect();
        }
        self.readable = ReadableLens::Available {
            name,
            source: source.to_owned(),
        };
        Ok(())
    }
}

pub(crate) fn project(
    document: &Document,
    graph: &Graph,
    _functions: &FunctionDeclarations,
    view: &mut WorkflowView,
    context: &crate::ViewContext,
) {
    view.callables = view
        .scopes
        .iter()
        .filter(|scope| matches!(scope.kind, ScopeKind::Entry | ScopeKind::Closure))
        .map(|scope| CallableView {
            site: if scope.kind == ScopeKind::Closure {
                graph
                    .node_at(&scope.block)
                    .and_then(|node| node.parent)
                    .map(|id| graph.node(id).site.clone())
                    .unwrap_or_else(|| scope.block.clone())
            } else {
                scope.block.clone()
            },
            block: scope.block.clone(),
        })
        .collect();
    let comparisons = crate::comparison::Comparisons::new(graph, &view.dataflow.bindings, context);
    let mut projection = Projection {
        view,
        nodes: Vec::new(),
        bearing: BTreeSet::new(),
        action_ordinals: document.action_ordinals(),
        condition_ordinals: document.condition_ordinals(),
        headers: graph
            .nodes()
            .iter()
            .filter_map(|node| match &node.kind {
                NodeKind::Stmt(
                    StmtNode::If { condition, .. } | StmtNode::While { condition, .. },
                ) => Some((
                    node.site.clone(),
                    (
                        HeaderRole::Condition,
                        graph.node(*condition).site.clone(),
                        comparisons.condition(node.id, *condition),
                    ),
                )),
                NodeKind::Stmt(StmtNode::For { iterable, .. }) => Some((
                    node.site.clone(),
                    (
                        HeaderRole::Iterable,
                        graph.node(*iterable).site.clone(),
                        None,
                    ),
                )),
                _ => None,
            })
            .collect(),
        writes: graph
            .nodes()
            .iter()
            .filter_map(|node| match &node.kind {
                NodeKind::Stmt(StmtNode::Assign { place, .. }) => {
                    written_binding(graph, place).map(|binding| (node.site.clone(), binding))
                }
                NodeKind::Stmt(StmtNode::Remove { member }) => {
                    written_member_binding(graph, member)
                        .map(|binding| (node.site.clone(), binding))
                }
                _ => None,
            })
            .collect(),
    };
    for statement in &projection.view.statements {
        if !statement.actions.is_empty() {
            projection.bearing.insert(statement.site.clone());
            let mut block = statement.block.clone();
            while let Some(scope) = projection
                .view
                .scopes
                .iter()
                .find(|scope| scope.block == block)
            {
                if matches!(scope.kind, ScopeKind::Entry | ScopeKind::Closure) {
                    break;
                }
                let Some(owner) = &scope.statement else {
                    break;
                };
                projection.bearing.insert(owner.clone());
                let Some(parent) = projection
                    .view
                    .statements
                    .iter()
                    .find(|statement| statement.site == *owner)
                else {
                    break;
                };
                block = parent.block.clone();
            }
        }
    }
    for callable in &projection.view.callables {
        projection.block(&callable.block, &callable.site);
    }
    view.nodes = projection.nodes;
    view.flow = graph
        .edges()
        .iter()
        .filter(|edge| graph.node(edge.from).site.unit == view.callables[0].site.unit)
        .map(|edge| FlowEdge {
            from: graph.node(edge.from).site.clone(),
            to: match edge.to {
                Target::Node(id) => FlowTarget::Site(graph.node(id).site.clone()),
                Target::Leave => FlowTarget::Leave,
                Target::End => FlowTarget::End,
            },
            kind: match edge.kind {
                EdgeKind::Next => FlowKind::Next,
                EdgeKind::Then => FlowKind::Then,
                EdgeKind::Else => FlowKind::Else,
                EdgeKind::Body => FlowKind::Body,
                EdgeKind::Break => FlowKind::Break,
                EdgeKind::Continue => FlowKind::Continue,
                EdgeKind::Return => FlowKind::Return,
                EdgeKind::Throw => FlowKind::Throw,
                EdgeKind::Catch => FlowKind::Catch,
                EdgeKind::Finally => FlowKind::Finally,
                EdgeKind::End => FlowKind::End,
                EdgeKind::Call => FlowKind::Call,
                EdgeKind::Spawn => FlowKind::Spawn,
                EdgeKind::Reference => FlowKind::Reference,
            },
        })
        .collect();
}

struct Projection<'a> {
    view: &'a WorkflowView,
    nodes: Vec<WorkflowNode>,
    bearing: BTreeSet<Site>,
    writes: BTreeMap<Site, usize>,
    action_ordinals: BTreeMap<Site, u32>,
    condition_ordinals: BTreeMap<Site, u32>,
    headers: BTreeMap<Site, (HeaderRole, Site, Option<ComparisonView>)>,
}

impl Projection<'_> {
    fn block(&mut self, block: &Site, callable: &Site) {
        let statements: Vec<_> = self
            .view
            .statements
            .iter()
            .filter(|statement| statement.block == *block)
            .collect();
        let mut run = Vec::new();
        for statement in statements {
            if !self.bearing.contains(&statement.site) {
                run.push(statement);
                continue;
            }
            self.code(block, callable, &run);
            run.clear();
            let arms: Vec<_> = self
                .view
                .scopes
                .iter()
                .filter(|scope| {
                    scope.statement.as_ref() == Some(&statement.site)
                        && !matches!(scope.kind, ScopeKind::Closure | ScopeKind::Entry)
                })
                .map(|scope| ControlArm {
                    role: scope.kind,
                    block: scope.block.clone(),
                })
                .collect();
            let control = matches!(
                statement.statement,
                Stmt::If { .. } | Stmt::For { .. } | Stmt::While { .. } | Stmt::Try { .. }
            );
            if control {
                let node = self.node(
                    WorkflowNodeBody::Control {
                        members: vec![statement.site.clone()],
                        condition_ordinal: self.condition_ordinals.get(&statement.site).copied(),
                        header: self.headers.get(&statement.site).map(
                            |(role, expression, comparison)| {
                                Box::new(ControlHeader {
                                    role: *role,
                                    expression: expression.clone(),
                                    actions: statement.actions.clone(),
                                    comparison: comparison.clone(),
                                })
                            },
                        ),
                        arms: arms.clone(),
                    },
                    statement.site.clone(),
                    block,
                    callable,
                    statement,
                );
                self.nodes.push(node);
            }
            for (index, action) in statement.actions.iter().enumerate() {
                let members = if !control && index == 0 {
                    vec![statement.site.clone()]
                } else {
                    Vec::new()
                };
                let region = self
                    .headers
                    .get(&statement.site)
                    .map_or_else(|| block.clone(), |(_, expression, _)| expression.clone());
                let within = self
                    .view
                    .execution_sites
                    .iter()
                    .find(|site| site.site == *action)
                    .and_then(|site| site.within.clone());
                let lazy = self
                    .view
                    .dataflow
                    .expressions
                    .iter()
                    .filter_map(|expression| {
                        if self.callable(&expression.block) != Some(callable)
                            || !matches!(expression.expression, Expr::If { .. })
                            || !under(action, &expression.site)
                        {
                            return None;
                        }
                        let arm = *action.path.get(expression.site.path.len())? as usize;
                        (arm == 1 || arm == 2).then(|| LazyArm {
                            expression: expression.site.clone(),
                            arm,
                        })
                    })
                    .collect();
                let node = self.node(
                    WorkflowNodeBody::Action {
                        members,
                        within,
                        lazy,
                    },
                    action.clone(),
                    &region,
                    callable,
                    statement,
                );
                self.nodes.push(node);
            }
            for arm in arms {
                self.block(&arm.block, callable);
            }
        }
        self.code(block, callable, &run);
    }

    fn code(&mut self, block: &Site, callable: &Site, run: &[&StatementView]) {
        let Some(first) = run.first() else {
            return;
        };
        // Descendant pure controls belong to the same group; closure bodies do
        // not. Use callable ownership, not a raw path-prefix fold.
        let mut members = NonEmptySites::new(first.site.clone());
        members.0.extend(
            self.view
                .statements
                .iter()
                .filter(|statement| {
                    statement.site != first.site
                        && run.iter().any(|root| under(&statement.site, &root.site))
                        && self.callable(&statement.block) == Some(callable)
                })
                .map(|statement| statement.site.clone()),
        );
        let node = self.node(
            WorkflowNodeBody::Code { members },
            first.site.clone(),
            block,
            callable,
            first,
        );
        self.nodes.push(node);
    }

    fn callable(&self, block: &Site) -> Option<&Site> {
        self.view
            .callables
            .iter()
            .filter(|callable| under(block, &callable.block))
            .max_by_key(|callable| callable.block.path.len())
            .map(|callable| &callable.site)
    }

    fn node(
        &self,
        body: WorkflowNodeBody,
        site: Site,
        block: &Site,
        callable: &Site,
        statement: &StatementView,
    ) -> WorkflowNode {
        let members = body.members();
        let owns = |candidate: &Site| {
            if matches!(body, WorkflowNodeBody::Code { .. }) {
                members.iter().any(|member| under(candidate, member))
            } else {
                under(candidate, &statement.site)
            }
        };
        let bindings: Vec<_> = self
            .view
            .dataflow
            .bindings
            .iter()
            .filter(|binding| {
                owns(&binding.site) && self.callable(&binding.block) == Some(callable)
            })
            .map(|binding| binding.id)
            .collect();
        let reads = self
            .view
            .dataflow
            .bindings
            .iter()
            .filter(|binding| {
                binding.uses.iter().any(|usage| {
                    if matches!(body, WorkflowNodeBody::Code { .. }) {
                        members.contains(&usage.statement)
                    } else if matches!(body, WorkflowNodeBody::Action { .. }) {
                        under(&usage.site, &site)
                    } else {
                        usage.statement == statement.site
                    }
                })
            })
            .map(|binding| binding.id)
            .collect();
        let writes = self
            .writes
            .iter()
            .filter(|(written, _)| {
                if matches!(body, WorkflowNodeBody::Code { .. }) {
                    members.contains(written)
                } else {
                    **written == statement.site
                }
            })
            .map(|(_, binding)| *binding)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let departures = if matches!(body, WorkflowNodeBody::Code { .. }) {
            self.view
                .statements
                .iter()
                .filter(|statement| {
                    members.contains(&statement.site) && terminal(&statement.statement)
                })
                .map(|statement| statement.site.clone())
                .collect()
        } else if terminal(&statement.statement) && statement.actions.last() == Some(&site) {
            vec![statement.site.clone()]
        } else {
            Vec::new()
        };
        let falls_through = if matches!(body, WorkflowNodeBody::Code { .. }) {
            self.view
                .statements
                .iter()
                .filter(|statement| statement.block == *block && members.contains(&statement.site))
                .all(|statement| falls_through(&statement.statement))
        } else if matches!(body, WorkflowNodeBody::Control { .. }) {
            falls_through(&statement.statement)
        } else {
            !terminal(&statement.statement) || statement.actions.last() != Some(&site)
        };

        let ordinal = if matches!(body, WorkflowNodeBody::Action { .. }) {
            self.action_ordinals[&site]
        } else {
            statement.ordinal
        };
        WorkflowNode {
            body,
            ordinal,
            site,
            block: block.clone(),
            callable: callable.clone(),
            statement: statement.site.clone(),
            bindings,
            reads,
            writes,
            readable: Vec::new(),
            folded: Vec::new(),
            departures,
            falls_through,
        }
    }
}

fn under(site: &Site, parent: &Site) -> bool {
    site.unit == parent.unit && site.path.starts_with(&parent.path)
}

fn lines(source: &str, span: Span) -> LineRange {
    let line = |offset: usize| {
        source.as_bytes()[..offset]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count()
            + 1
    };
    LineRange {
        first: line(span.start),
        last: line(span.end.saturating_sub(1).max(span.start)),
    }
}

fn terminal(statement: &Stmt) -> bool {
    matches!(
        statement,
        Stmt::Return { .. }
            | Stmt::ReturnVia { .. }
            | Stmt::Throw { .. }
            | Stmt::Finish { .. }
            | Stmt::Fail { .. }
            | Stmt::Break
            | Stmt::Continue
    )
}
pub(crate) fn falls_through(statement: &Stmt) -> bool {
    let block = |statements: &[Stmt]| statements.iter().all(falls_through);
    match statement {
        Stmt::If {
            then_block,
            else_block,
            ..
        } => block(then_block) || block(else_block),
        Stmt::Try(scope) => {
            scope.finally.as_ref().is_none_or(|finally| block(finally))
                && (block(&scope.body)
                    || scope.catch.as_ref().is_some_and(|catch| block(&catch.body)))
        }
        _ => !terminal(statement),
    }
}

fn written_binding(graph: &Graph, place: &PlaceNode) -> Option<usize> {
    match place {
        PlaceNode::Variable(Reference::Binding(binding)) => Some(binding.index()),
        PlaceNode::Variable(Reference::Session(_)) => None,
        PlaceNode::Member(member) => written_member_binding(graph, member),
    }
}

fn written_member_binding(graph: &Graph, member: &MemberNode) -> Option<usize> {
    let mut target = match member {
        MemberNode::Field { target, .. } | MemberNode::Index { target, .. } => *target,
    };
    loop {
        match &graph.node(target).kind {
            NodeKind::Expr(ExprNode::Variable(Reference::Binding(binding))) => {
                return Some(binding.index());
            }
            NodeKind::Expr(ExprNode::Member(
                MemberNode::Field { target: parent, .. } | MemberNode::Index { target: parent, .. },
            )) => target = *parent,
            _ => return None,
        }
    }
}
