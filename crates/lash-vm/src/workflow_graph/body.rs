//! How a body holds its statements.
//!
//! The ownership walk ([`super::statement_list`]) reads a body's statements
//! through the structure around them: a plain block, a statement list closed
//! by a completion value, a statement that is itself such a list, or one bare
//! statement. A [`WorkflowBodyShape`] holds the statements in that structure,
//! so the body expression is rebuilt from it exactly, with no front end to
//! normalize it, and no shape it can take fails to spell a body.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ast::{Expr, StructuralRole};

use super::{WorkflowNode, deserialize_strict, workflow_node_statement};

/// The statements of a body, in execution order, as the IR spells them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "form", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowBodyShape {
    /// A list of statements. With `completion` the list is closed by that
    /// value ([`StructuralRole::Completion`]): the statements run in order
    /// and the list evaluates to it, a pure expression that is no statement.
    /// Without one it is a block.
    List {
        #[serde(default)]
        items: Vec<WorkflowBodyItem>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(deserialize_with = "deserialize_strict")]
        completion: Option<Box<Expr>>,
    },
    /// The body is its one statement, with no list around it.
    Statement { node: Box<WorkflowNode> },
}

impl Default for WorkflowBodyShape {
    fn default() -> Self {
        Self::List {
            items: Vec::new(),
            completion: None,
        }
    }
}

/// One entry of a statement list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowBodyItem {
    Node(Box<WorkflowNode>),
    /// A run of statements the IR holds as one nested statement list closed
    /// by its own completion value: the statement a front end gave a value.
    /// It may hold no statement at all.
    Group {
        #[serde(default)]
        items: Vec<WorkflowBodyItem>,
        #[serde(deserialize_with = "deserialize_strict")]
        value: Expr,
    },
}

impl WorkflowBodyShape {
    /// A block of `nodes`, none of them grouped.
    pub fn block(nodes: impl IntoIterator<Item = WorkflowNode>) -> Self {
        Self::List {
            items: nodes
                .into_iter()
                .map(|node| WorkflowBodyItem::Node(Box::new(node)))
                .collect(),
            completion: None,
        }
    }

    /// The statements in execution order, through every group.
    pub fn nodes(&self) -> Vec<&WorkflowNode> {
        fn collect<'a>(items: &'a [WorkflowBodyItem], out: &mut Vec<&'a WorkflowNode>) {
            for item in items {
                match item {
                    WorkflowBodyItem::Node(node) => out.push(node),
                    WorkflowBodyItem::Group { items, .. } => collect(items, out),
                }
            }
        }
        let mut out = Vec::new();
        match self {
            Self::List { items, .. } => collect(items, &mut out),
            Self::Statement { node } => out.push(node.as_ref()),
        }
        out
    }

    /// [`Self::nodes`], for changing them in place.
    pub fn nodes_mut(&mut self) -> Vec<&mut WorkflowNode> {
        fn collect<'a>(items: &'a mut [WorkflowBodyItem], out: &mut Vec<&'a mut WorkflowNode>) {
            for item in items {
                match item {
                    WorkflowBodyItem::Node(node) => out.push(node),
                    WorkflowBodyItem::Group { items, .. } => collect(items, out),
                }
            }
        }
        let mut out = Vec::new();
        match self {
            Self::List { items, .. } => collect(items, &mut out),
            Self::Statement { node } => out.push(node.as_mut()),
        }
        out
    }

    /// This shape holding `nodes` instead of its own statements, for a host
    /// that gives a body's statements again without giving its arrangement.
    ///
    /// When `nodes` are the same statements in the same order, by id, each
    /// takes the place of the one with its id and every group is kept. When
    /// they are not, every statement goes directly in the list, which keeps
    /// only its own completion value: a formerly grouped statement stays a
    /// statement and loses the value its front end closed it with. A
    /// single-statement body stays one while it holds one statement.
    pub fn with_nodes(&self, nodes: Vec<WorkflowNode>) -> Self {
        fn refill(
            items: &[WorkflowBodyItem],
            nodes: &mut impl Iterator<Item = WorkflowNode>,
        ) -> Vec<WorkflowBodyItem> {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    WorkflowBodyItem::Node(_) => {
                        out.extend(
                            nodes
                                .next()
                                .map(|node| WorkflowBodyItem::Node(Box::new(node))),
                        );
                    }
                    WorkflowBodyItem::Group { items, value } => out.push(WorkflowBodyItem::Group {
                        items: refill(items, nodes),
                        value: value.clone(),
                    }),
                }
            }
            out
        }
        let held = self.nodes();
        let same = held.len() == nodes.len()
            && held
                .iter()
                .zip(&nodes)
                .all(|(held, node)| held.id == node.id);
        let mut nodes = nodes.into_iter();
        match (self, nodes.len()) {
            (Self::Statement { .. }, 1) => match nodes.next() {
                Some(node) => Self::Statement {
                    node: Box::new(node),
                },
                None => Self::default(),
            },
            (Self::Statement { .. }, _) => Self::block(nodes),
            (Self::List { items, completion }, _) if same => Self::List {
                items: refill(items, &mut nodes),
                completion: completion.clone(),
            },
            (Self::List { completion, .. }, _) => Self::List {
                items: nodes
                    .map(|node| WorkflowBodyItem::Node(Box::new(node)))
                    .collect(),
                completion: completion.clone(),
            },
        }
    }

    /// The statements in execution order, taken out of the shape.
    pub fn into_nodes(self) -> Vec<WorkflowNode> {
        fn collect(items: Vec<WorkflowBodyItem>, out: &mut Vec<WorkflowNode>) {
            for item in items {
                match item {
                    WorkflowBodyItem::Node(node) => out.push(*node),
                    WorkflowBodyItem::Group { items, .. } => collect(items, out),
                }
            }
        }
        let mut out = Vec::new();
        match self {
            Self::List { items, .. } => collect(items, &mut out),
            Self::Statement { node } => out.push(*node),
        }
        out
    }

    /// The items of the body as a list, turning a single-statement body into
    /// the block of that statement, which runs the same way.
    pub fn items_mut(&mut self) -> &mut Vec<WorkflowBodyItem> {
        if let Self::Statement { node } = self {
            let node = node.clone();
            *self = Self::List {
                items: vec![WorkflowBodyItem::Node(node)],
                completion: None,
            };
        }
        match self {
            Self::List { items, .. } => items,
            Self::Statement { .. } => unreachable!("a single-statement body was made a list above"),
        }
    }

    /// Reads `body` the way [`super::statement_list`] reads its statements,
    /// taking each statement's node from `nodes` in that order.
    pub(super) fn of(body: &Expr, nodes: &mut impl Iterator<Item = WorkflowNode>) -> Self {
        match body {
            Expr::Role {
                role: StructuralRole::Completion,
                expr,
            } => match expr.as_ref() {
                Expr::Block(items) => match items.split_last() {
                    Some((value, statements)) => Self::List {
                        items: items_of(statements, nodes),
                        completion: Some(Box::new(value.clone())),
                    },
                    // A completion list with no value is no statement at
                    // all; the ownership walk skips it and `validate_ast`
                    // refuses it.
                    None => Self::default(),
                },
                _ => Self::default(),
            },
            Expr::Block(items) => Self::List {
                items: items_of(items, nodes),
                completion: None,
            },
            _ => match nodes.next() {
                Some(node) => Self::Statement {
                    node: Box::new(node),
                },
                None => Self::default(),
            },
        }
    }

    /// The body expression this shape spells.
    pub(super) fn expression(&self) -> Expr {
        match self {
            Self::Statement { node } => workflow_node_statement(node),
            Self::List { items, completion } => {
                let mut list = item_expressions(items);
                match completion {
                    None => Expr::Block(list),
                    Some(value) => {
                        list.push(value.as_ref().clone());
                        completion_list(list)
                    }
                }
            }
        }
    }
}

fn completion_list(items: Vec<Expr>) -> Expr {
    Expr::Role {
        role: StructuralRole::Completion,
        expr: Box::new(Expr::Block(items)),
    }
}

/// The entries of the statement list `items`, one node per visible statement.
fn items_of(
    items: &[Expr],
    nodes: &mut impl Iterator<Item = WorkflowNode>,
) -> Vec<WorkflowBodyItem> {
    let mut out = Vec::new();
    for item in items {
        let nested = match item {
            Expr::Role {
                role: StructuralRole::Completion,
                expr,
            } => match expr.as_ref() {
                Expr::Block(nested) => nested.split_last(),
                _ => None,
            },
            _ => {
                out.extend(
                    nodes
                        .next()
                        .map(|node| WorkflowBodyItem::Node(Box::new(node))),
                );
                continue;
            }
        };
        // A completion list with no value is no statement at all, as above.
        let Some((value, statements)) = nested else {
            continue;
        };
        out.push(WorkflowBodyItem::Group {
            items: items_of(statements, nodes),
            value: value.clone(),
        });
    }
    out
}

fn item_expressions(items: &[WorkflowBodyItem]) -> Vec<Expr> {
    items
        .iter()
        .map(|item| match item {
            WorkflowBodyItem::Node(node) => workflow_node_statement(node),
            WorkflowBodyItem::Group { items, value } => {
                let mut nested = item_expressions(items);
                nested.push(value.clone());
                completion_list(nested)
            }
        })
        .collect()
}
