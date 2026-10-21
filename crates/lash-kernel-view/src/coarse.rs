//! Canonical, revision-local coarse membership. No dialect or runtime executes here.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_check::{
    EdgeKind, ExprNode, Graph, MemberNode, NodeKind, PlaceNode, Reference, StmtNode, Target,
};
use lash_kernel_doc::Document;
use lash_kernel_doc::Expr;
use lash_kernel_doc::FunctionDeclarations;
use lash_kernel_doc::SourceMap;
use lash_kernel_doc::SourceMapRefusal;
use lash_kernel_doc::Span;
use lash_kernel_doc::Stmt;
use lash_vocabulary::sites::Site;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::comparison::{ComparisonView, Comparisons};
use crate::{ScopeKind, StatementView, WorkflowView};

/// A lexical callable is projected once, independently of its invocations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CallableView {
    pub site: Site,
    pub block: Site,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeKind {
    Action,
    Control,
    Code,
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

/// Identity is (view.document, kind, block, site, last member). Ranges are
/// display metadata. Supporting statements and dataflow remain addressable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowNode {
    pub kind: WorkflowNodeKind,
    pub site: Site,
    pub block: Site,
    pub callable: Site,
    /// The owning statement for actions/controls; first member for code.
    pub statement: Site,
    /// The node's authoritative count coordinate in RanCounts.
    pub ordinal: u32,
    pub condition_ordinal: Option<u32>,
    /// Each statement has exactly one carrier. Additional actions in a
    /// statement have no members, but retain their owning `statement`.
    pub members: Vec<Site>,
    pub bindings: Vec<usize>,
    pub reads: Vec<usize>,
    pub writes: Vec<usize>,
    pub canonical: LineRange,
    pub readable: Vec<LineRange>,
    pub within: Option<Site>,
    pub lazy: Vec<LazyArm>,
    pub header: Option<ControlHeader>,
    pub arms: Vec<ControlArm>,
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

/// A failed readable lens does not invalidate canonical membership or ranges.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReadableLens {
    Unrequested,
    Available {
        name: String,
    },
    Refused {
        name: String,
        cause: ReadableRefusal,
    },
}

/// Typed printer refusal. Projection and the canonical lens remain available.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReadableRefusal {
    Bytes { limit: usize },
    Depth { limit: usize },
    Encoding { message: String },
    Invalid { message: String },
}

impl WorkflowView {
    /// Exact action-site attribution, also for a nested operand whose enclosing
    /// action has not issued. Occurrences and task identities stay with the
    /// observation; a host never attributes by the owning statement.
    pub fn node_at(&self, site: &Site) -> Option<&WorkflowNode> {
        self.nodes
            .iter()
            .find(|node| node.site == *site)
            .or_else(|| self.nodes.iter().find(|node| node.members.contains(site)))
    }

    pub fn action_node(
        &self,
        site: &lash_vocabulary::sites::PositionLabel,
    ) -> Option<&WorkflowNode> {
        self.nodes
            .iter()
            .find(|node| node.kind == WorkflowNodeKind::Action && node.site == *site)
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
            .map(|entry| (&entry.site, &entry.ranges))
            .collect();
        for node in &mut self.nodes {
            let sites = if node.kind == WorkflowNodeKind::Code {
                node.members.clone()
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
        self.readable = ReadableLens::Available { name };
        Ok(())
    }
}

pub(crate) fn project(
    document: &Document,
    graph: &Graph,
    functions: &FunctionDeclarations,
    view: &mut WorkflowView,
    packages: &lash_kernel_package::PackageView<'_>,
) {
    let (source, ranges) = lash_kernel_doc::print_document_with_ranges(document);
    view.canonical_source = source.clone();
    view.callables = view
        .scopes
        .iter()
        .filter(|scope| matches!(scope.kind, ScopeKind::Entry | ScopeKind::Closure))
        .map(|scope| CallableView {
            site: if scope.kind == ScopeKind::Closure {
                Site::new(
                    scope.block.unit.clone(),
                    scope.block.path[..scope.block.path.len() - 1].to_vec(),
                )
            } else {
                scope.block.clone()
            },
            block: scope.block.clone(),
        })
        .collect();
    let comparisons = Comparisons::new(graph, functions, &view.dataflow.bindings, packages);
    let mut projection = Projection {
        view,
        source: &source,
        ranges: &ranges,
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
    source: &'a str,
    ranges: &'a BTreeMap<Site, Span>,
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
                let mut node = self.node(
                    WorkflowNodeKind::Control,
                    statement.site.clone(),
                    block,
                    callable,
                    statement,
                    vec![statement.site.clone()],
                );
                node.header =
                    self.headers
                        .get(&statement.site)
                        .map(|(role, expression, comparison)| ControlHeader {
                            role: *role,
                            expression: expression.clone(),
                            actions: statement.actions.clone(),
                            comparison: comparison.clone(),
                        });
                node.arms = arms.clone();
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
                let mut node = self.node(
                    WorkflowNodeKind::Action,
                    action.clone(),
                    &region,
                    callable,
                    statement,
                    members,
                );
                node.within = self
                    .view
                    .execution_sites
                    .iter()
                    .find(|site| site.site == *action)
                    .and_then(|site| site.within.clone());
                node.lazy = self
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
        let members: Vec<_> = self
            .view
            .statements
            .iter()
            .filter(|statement| {
                run.iter().any(|root| under(&statement.site, &root.site))
                    && self.callable(&statement.block) == Some(callable)
            })
            .map(|statement| statement.site.clone())
            .collect();
        let node = self.node(
            WorkflowNodeKind::Code,
            first.site.clone(),
            block,
            callable,
            first,
            members,
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
        kind: WorkflowNodeKind,
        site: Site,
        block: &Site,
        callable: &Site,
        statement: &StatementView,
        members: Vec<Site>,
    ) -> WorkflowNode {
        let span_sites = if kind == WorkflowNodeKind::Code {
            members.clone()
        } else {
            vec![statement.site.clone()]
        };
        let spans: Vec<_> = span_sites
            .iter()
            .map(|site| lines(self.source, self.ranges[site]))
            .collect();
        let canonical = LineRange {
            first: spans.iter().map(|span| span.first).min().unwrap_or(1),
            last: spans.iter().map(|span| span.last).max().unwrap_or(1),
        };
        let owns = |candidate: &Site| {
            if kind == WorkflowNodeKind::Code {
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
                    if kind == WorkflowNodeKind::Code {
                        members.contains(&usage.statement)
                    } else if kind == WorkflowNodeKind::Action {
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
                if kind == WorkflowNodeKind::Code {
                    members.contains(written)
                } else {
                    **written == statement.site
                }
            })
            .map(|(_, binding)| *binding)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let departures = if kind == WorkflowNodeKind::Code {
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
        let falls_through = if kind == WorkflowNodeKind::Code {
            self.view
                .statements
                .iter()
                .filter(|statement| statement.block == *block && members.contains(&statement.site))
                .all(|statement| falls_through(&statement.statement))
        } else if kind == WorkflowNodeKind::Control {
            falls_through(&statement.statement)
        } else {
            !terminal(&statement.statement) || statement.actions.last() != Some(&site)
        };

        let ordinal = if kind == WorkflowNodeKind::Action {
            self.action_ordinals[&site]
        } else {
            statement.ordinal
        };
        let condition_ordinal = (kind == WorkflowNodeKind::Control)
            .then(|| self.condition_ordinals.get(&statement.site).copied())
            .flatten();
        WorkflowNode {
            kind,
            ordinal,
            condition_ordinal,
            site,
            block: block.clone(),
            callable: callable.clone(),
            statement: statement.site.clone(),
            members,
            bindings,
            reads,
            writes,
            canonical,
            readable: Vec::new(),
            within: None,
            lazy: Vec::new(),
            header: None,
            arms: Vec::new(),
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
