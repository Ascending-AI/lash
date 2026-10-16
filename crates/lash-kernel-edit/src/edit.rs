//! Canvas batches and private tree mutations.
use crate::authoring::Operation;
use lash_kernel_doc::{DocumentId, Expr, Name, Site, Stmt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// A canvas batch applied whole or not at all. Every operation addresses this base.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Transaction {
    pub base: DocumentId,
    pub edits: Vec<Operation>,
}
impl Transaction {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// Where a statement is put: in `block`, ahead of the statement `before`,
/// or at the block's end.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub block: Site,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Site>,
}

impl Position {
    /// The end of `block`.
    pub fn end(block: Site) -> Self {
        Self {
            block,
            before: None,
        }
    }

    /// Ahead of the statement `before`, in the block that holds it.
    ///
    /// Returns `None` when `before` is a unit's body, which no block holds.
    pub fn before(before: Site) -> Option<Self> {
        let mut block = before.clone();
        block.path.pop()?;
        Some(Self {
            block,
            before: Some(before),
        })
    }
}

/// Tree writes are private. Extraction alone uses the declaration cleanup writes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Edit {
    InsertStatement {
        at: Position,
        statement: Stmt,
    },
    RemoveStatement {
        statement: Site,
    },
    MoveStatement {
        statement: Site,
        to: Position,
    },
    ReplaceExpression {
        expression: Site,
        with: Expr,
    },
    SetArgument {
        action: Site,
        index: u32,
        argument: Expr,
    },
    SetCondition {
        statement: Site,
        condition: Expr,
    },
    RemoveFunction {
        name: Name,
    },
    SetPrivateBindings {
        names: BTreeSet<Name>,
    },
    RemoveEntry {
        function: Name,
    },
}
