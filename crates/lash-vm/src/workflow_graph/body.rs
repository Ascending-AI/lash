//! How the IR spells an ordered body of statements.
//!
//! The ownership walk ([`super::statement_list`]) reads a body's statements
//! through the structure around them: a plain block, a statement list closed
//! by a completion value, a statement that is itself such a list, or one bare
//! statement. A [`WorkflowBodyForm`] records that structure, so the body
//! expression is rebuilt from its nodes exactly, with no front end to
//! normalize it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ast::{Expr, StructuralRole};

use super::{WorkflowGraphError, deserialize_strict};

/// The IR spelling of a body whose statements are a subgraph's nodes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "form", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowBodyForm {
    /// A block of the statements.
    Block {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        groups: Vec<WorkflowCompletionGroup>,
    },
    /// A statement list closed by a completion value
    /// ([`StructuralRole::Completion`]): the statements run in order and the
    /// list evaluates to `value`, a pure expression that is no statement.
    Completion {
        #[serde(deserialize_with = "deserialize_strict")]
        value: Box<Expr>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        groups: Vec<WorkflowCompletionGroup>,
    },
    /// The body is its one statement, with no list around it.
    Statement,
}

impl Default for WorkflowBodyForm {
    fn default() -> Self {
        Self::Block { groups: Vec::new() }
    }
}

/// A run of consecutive statements that the IR holds as one nested statement
/// list closed by its own completion value: the statement a front end gave a
/// value. It covers the nodes `start .. start + len` of its body; groups are
/// ordered, never overlap, and nest through `groups`, whose ranges lie inside
/// this one. A group may cover no statement at all.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCompletionGroup {
    pub start: u32,
    pub len: u32,
    #[serde(deserialize_with = "deserialize_strict")]
    pub value: Expr,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<WorkflowCompletionGroup>,
}

impl WorkflowBodyForm {
    /// The same form with every statement directly in the list.
    ///
    /// Groups name their statements by position, so a host that adds, removes
    /// or reorders a body's nodes without restating them uses this: each
    /// formerly grouped statement stays a statement and loses only the value
    /// its front end closed it with.
    pub fn ungrouped(&self) -> Self {
        match self {
            Self::Block { .. } => Self::Block { groups: Vec::new() },
            Self::Completion { value, .. } => Self::Completion {
                value: value.clone(),
                groups: Vec::new(),
            },
            Self::Statement => Self::Statement,
        }
    }

    /// The form of `body`, read the way [`super::statement_list`] reads its
    /// statements.
    pub(super) fn of(body: &Expr) -> Self {
        match body {
            Expr::Role {
                role: StructuralRole::Completion,
                expr,
            } => match expr.as_ref() {
                Expr::Block(items) => match items.split_last() {
                    Some((value, statements)) => Self::Completion {
                        value: Box::new(value.clone()),
                        groups: groups_of(statements, &mut 0),
                    },
                    None => Self::Statement,
                },
                _ => Self::Statement,
            },
            Expr::Block(items) => Self::Block {
                groups: groups_of(items, &mut 0),
            },
            _ => Self::Statement,
        }
    }

    /// The body expression this form spells around `statements`, one per
    /// node in order.
    pub(super) fn body(&self, statements: Vec<Expr>) -> Result<Expr, WorkflowGraphError> {
        let count = statements.len();
        let mut statements = statements.into_iter();
        match self {
            Self::Statement => match (statements.next(), statements.next()) {
                (Some(statement), None) => Ok(statement),
                _ => Err(WorkflowGraphError::InvalidBodyForm {
                    message: format!("a single-statement body holds one node, found {count}"),
                }),
            },
            Self::Block { groups } => Ok(Expr::Block(grouped(&mut statements, groups, 0, count)?)),
            Self::Completion { value, groups } => {
                let mut items = grouped(&mut statements, groups, 0, count)?;
                items.push(value.as_ref().clone());
                Ok(Expr::Role {
                    role: StructuralRole::Completion,
                    expr: Box::new(Expr::Block(items)),
                })
            }
        }
    }
}

/// The completion groups among `items`, with `next` the index of the next
/// visible statement.
fn groups_of(items: &[Expr], next: &mut u32) -> Vec<WorkflowCompletionGroup> {
    let mut groups = Vec::new();
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
                *next += 1;
                continue;
            }
        };
        // A completion list with no value is no statement at all; the
        // ownership walk skips it and `validate_ast` refuses it.
        let Some((value, statements)) = nested else {
            continue;
        };
        let start = *next;
        let inner = groups_of(statements, next);
        groups.push(WorkflowCompletionGroup {
            start,
            len: *next - start,
            value: value.clone(),
            groups: inner,
        });
    }
    groups
}

/// The items of the list covering statements `start .. end`: each ungrouped
/// statement as itself and each group as its nested completion list.
fn grouped(
    statements: &mut std::vec::IntoIter<Expr>,
    groups: &[WorkflowCompletionGroup],
    start: usize,
    end: usize,
) -> Result<Vec<Expr>, WorkflowGraphError> {
    let malformed = || WorkflowGraphError::InvalidBodyForm {
        message: "completion groups are ordered ranges inside their body".to_string(),
    };
    let mut items = Vec::new();
    let mut cursor = start;
    for group in groups {
        let group_start = group.start as usize;
        let group_end = group_start
            .checked_add(group.len as usize)
            .ok_or_else(malformed)?;
        if group_start < cursor || group_end > end {
            return Err(malformed());
        }
        items.extend(statements.by_ref().take(group_start - cursor));
        let mut nested = grouped(statements, &group.groups, group_start, group_end)?;
        nested.push(group.value.clone());
        items.push(Expr::Role {
            role: StructuralRole::Completion,
            expr: Box::new(Expr::Block(nested)),
        });
        cursor = group_end;
    }
    items.extend(statements.by_ref().take(end - cursor));
    Ok(items)
}
