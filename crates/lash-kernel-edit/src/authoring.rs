//! The closed canvas vocabulary. Source replacement owns all other edits.
use crate::Position;
use lash_kernel_doc::{Expr, Literal, MapEntry, RecordEntry};

use lash_vocabulary::names::DocumentId;

use lash_vocabulary::names::Name;
use lash_vocabulary::types::Type;

use lash_vocabulary::sites::Site;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Data that can be connected to a value slot; no executable syntax.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum DataValue {
    Literal(Literal),
    Variable(Name),
    Tuple(Vec<DataValue>),
    List(Vec<DataValue>),
    Set(Vec<DataValue>),
    Map(Vec<DataMapEntry>),
    Record(Vec<DataRecordEntry>),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DataMapEntry {
    pub key: DataValue,
    pub value: DataValue,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DataRecordEntry {
    pub field: String,
    pub value: DataValue,
}

impl DataValue {
    pub(crate) fn expression(&self) -> Expr {
        match self {
            Self::Literal(value) => Expr::Literal(value.clone()),
            Self::Variable(name) => Expr::Variable(name.clone()),
            Self::Tuple(items) => Expr::Tuple(items.iter().map(Self::expression).collect()),
            Self::List(items) => Expr::List(items.iter().map(Self::expression).collect()),
            Self::Set(items) => Expr::Set(items.iter().map(Self::expression).collect()),
            Self::Map(items) => Expr::Map(
                items
                    .iter()
                    .map(|item| MapEntry {
                        key: item.key.expression(),
                        value: item.value.expression(),
                    })
                    .collect(),
            ),
            Self::Record(items) => Expr::Record(
                items
                    .iter()
                    .map(|item| RecordEntry {
                        field: item.field.clone(),
                        value: item.value.expression(),
                    })
                    .collect(),
            ),
        }
    }
}

/// All addresses are exact sites in the operation's base document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueTarget {
    Literal {
        expression: Site,
    },
    Initializer {
        statement: Site,
    },
    ToolArgument {
        action: Site,
        index: u32,
        /// Canonical child indices within the argument, empty for its root.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        path: Vec<u32>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Wrapper {
    If {
        condition: Expr,
    },
    For {
        binding: Name,
        iterable: Expr,
    },
    /// A new try has an empty catch and no finally.
    Try {
        catch_binding: Name,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Branch {
    Then,
    Else,
    Body,
}

/// Every supported canvas operation, admitted atomically through [`crate::Draft::apply`].
/// Structural operations accept only separable perform/tool steps. While loops
/// support condition editing; creating or removing one requires source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    SetValue {
        base: DocumentId,
        target: ValueTarget,
        value: DataValue,
    },
    InsertStep {
        base: DocumentId,
        at: Position,
        slot: lash_kernel_package::ImportSlot,
        args: Vec<DataValue>,
        result: Type,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<Name>,
    },
    RemoveStep {
        base: DocumentId,
        action: Site,
    },
    MoveStep {
        base: DocumentId,
        action: Site,
        to: Position,
    },
    WrapSteps {
        base: DocumentId,
        first: Site,
        last: Site,
        wrapper: Wrapper,
    },
    UnwrapSteps {
        base: DocumentId,
        wrapper: Site,
        branch: Branch,
    },
    ChangeCondition {
        base: DocumentId,
        statement: Site,
        condition: Expr,
    },
}
impl Operation {
    pub fn base(&self) -> DocumentId {
        match self {
            Self::SetValue { base, .. }
            | Self::InsertStep { base, .. }
            | Self::RemoveStep { base, .. }
            | Self::MoveStep { base, .. }
            | Self::WrapSteps { base, .. }
            | Self::UnwrapSteps { base, .. }
            | Self::ChangeCondition { base, .. } => *base,
        }
    }
}
