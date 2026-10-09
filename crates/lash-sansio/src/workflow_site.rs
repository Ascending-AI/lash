use std::num::NonZeroU64;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The role one child expression plays in its parent expression of the
/// shared workflow IR.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExprSlot {
    /// A statement of a block or an element of a list.
    Item(u32),
    /// The value of a record entry, by entry position.
    Entry(u32),
    /// The expression a label or a structural role wraps.
    Inner,
    /// A dynamic index of an assignment target's path, by position.
    AssignIndex(u32),
    /// The value an assignment stores.
    Value,
    Condition,
    Then,
    Else,
    Iterable,
    /// The generated statements that bind a loop element to authored names.
    Bind,
    /// The body of a loop, a function, a process literal or a `try`.
    Body,
    /// The input of a host descriptor constructor.
    Input,
    Receiver,
    /// A call argument, by position.
    Arg(u32),
    /// The single operand of a unary form.
    Operand,
    Callee,
    /// The computed member a method call reads its callee from.
    MethodKey,
    This,
    /// The collection a map intrinsic reads.
    Items,
    /// The callback a map intrinsic applies.
    Function,
    Catch,
    Finally,
    Target,
    Index,
    Left,
    Right,
}

impl std::fmt::Display for ExprSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Item(index) => return write!(formatter, "item[{index}]"),
            Self::Entry(index) => return write!(formatter, "entry[{index}]"),
            Self::AssignIndex(index) => return write!(formatter, "assign_index[{index}]"),
            Self::Arg(index) => return write!(formatter, "arg[{index}]"),
            Self::Inner => "inner",
            Self::Value => "value",
            Self::Condition => "condition",
            Self::Then => "then",
            Self::Else => "else",
            Self::Iterable => "iterable",
            Self::Bind => "bind",
            Self::Body => "body",
            Self::Input => "input",
            Self::Receiver => "receiver",
            Self::Operand => "operand",
            Self::Callee => "callee",
            Self::MethodKey => "method_key",
            Self::This => "this",
            Self::Items => "items",
            Self::Function => "function",
            Self::Catch => "catch",
            Self::Finally => "finally",
            Self::Target => "target",
            Self::Index => "index",
            Self::Left => "left",
            Self::Right => "right",
        };
        formatter.write_str(name)
    }
}

/// What a site that is not an expression of its own stands for.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSiteRole {
    /// The step a label declares over the expression it wraps, when that
    /// expression is not itself the labeled operation.
    LabeledStep,
}

/// A deterministic node identifier of a workflow document, minted from the
/// node's structural owner and AST path. It is never empty.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct WorkflowNodeId(String);

/// A node id was empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a workflow node id is never empty")]
pub struct EmptyWorkflowNodeId;

impl WorkflowNodeId {
    /// The id spelled `id`, as a document or a stored record names it.
    pub fn new(id: impl Into<String>) -> Result<Self, EmptyWorkflowNodeId> {
        let id = id.into();
        if id.is_empty() {
            return Err(EmptyWorkflowNodeId);
        }
        Ok(Self(id))
    }

    /// The id a projector mints from the hex digest of a node's preimage.
    pub fn from_digest_hex(digest: &str) -> Self {
        Self(format!("node:{digest}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The node id spelled `label` in a test fixture.
    ///
    /// # Panics
    ///
    /// When `label` is empty.
    #[doc(hidden)]
    pub fn fixture(label: &str) -> Self {
        assert!(!label.is_empty(), "a fixture node id is never empty");
        Self(label.to_owned())
    }
}

impl std::fmt::Display for WorkflowNodeId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for WorkflowNodeId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for WorkflowNodeId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for WorkflowNodeId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for WorkflowNodeId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl std::str::FromStr for WorkflowNodeId {
    type Err = EmptyWorkflowNodeId;

    fn from_str(id: &str) -> Result<Self, Self::Err> {
        Self::new(id)
    }
}

impl<'de> Deserialize<'de> for WorkflowNodeId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// The typed path from a workflow node's statement to one expression inside
/// it: the child slot taken at each step. The empty path is the statement
/// itself.
///
/// The serialized list is authoritative. [`Display`](std::fmt::Display) is a
/// derived spelling for text-only host contracts.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct WorkflowSlotPath(pub Vec<ExprSlot>);

impl WorkflowSlotPath {
    /// The path through `slots`, from a node's statement.
    pub fn new(slots: impl IntoIterator<Item = ExprSlot>) -> Self {
        Self(slots.into_iter().collect())
    }

    pub fn slots(&self) -> &[ExprSlot] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<ExprSlot> for WorkflowSlotPath {
    fn from_iter<I: IntoIterator<Item = ExprSlot>>(slots: I) -> Self {
        Self::new(slots)
    }
}

impl std::fmt::Display for WorkflowSlotPath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for slot in &self.0 {
            write!(formatter, "/{slot}")?;
        }
        Ok(())
    }
}

/// The address of one execution site inside a workflow node: the slot path
/// from the node's statement to the site's expression, and the synthetic
/// site of that expression it names, if it is not the expression's own. The
/// default is the statement's own site.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSitePath {
    #[serde(default, skip_serializing_if = "WorkflowSlotPath::is_empty")]
    pub slots: WorkflowSlotPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<WorkflowSiteRole>,
}

impl WorkflowSitePath {
    /// The own site of the expression `slots` reach from a node's statement.
    pub fn at(slots: impl IntoIterator<Item = ExprSlot>) -> Self {
        Self {
            slots: WorkflowSlotPath::new(slots),
            role: None,
        }
    }

    /// The synthetic `role` site of the expression this path reaches.
    #[must_use]
    pub fn with_role(mut self, role: WorkflowSiteRole) -> Self {
        self.role = Some(role);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty() && self.role.is_none()
    }
}

impl std::fmt::Display for WorkflowSitePath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.slots)?;
        match self.role {
            Some(WorkflowSiteRole::LabeledStep) => formatter.write_str("#labeled_step"),
            None => Ok(()),
        }
    }
}

/// The static address of one execution site in a workflow document: the node
/// that owns it and the path to the executable subexpression inside it.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSiteRef {
    pub node_id: WorkflowNodeId,
    #[serde(default, skip_serializing_if = "WorkflowSitePath::is_empty")]
    pub site_path: WorkflowSitePath,
}

impl WorkflowSiteRef {
    pub fn new(node_id: WorkflowNodeId, site_path: WorkflowSitePath) -> Self {
        Self { node_id, site_path }
    }

    /// The node's own statement site.
    pub fn node(node_id: WorkflowNodeId) -> Self {
        Self::new(node_id, WorkflowSitePath::default())
    }
}

impl std::fmt::Display for WorkflowSiteRef {
    /// The node id followed by the site path's spelling: a node's own
    /// statement site spells as the bare node id.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}{}", self.node_id, self.site_path)
    }
}

/// One execution site of a workflow node as its document lists it: the exact
/// executable subexpression inside the node's statement and a description of
/// what runs there. The node that lists it is its owner; `kind` and `label`
/// describe the site and are not its identity.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSiteDescriptor {
    #[serde(default, skip_serializing_if = "WorkflowSitePath::is_empty")]
    pub site_path: WorkflowSitePath,
    pub kind: crate::ExecutionNodeKind,
    pub label: String,
}

/// Where inside one loop activation something ran. Numbers are one-based.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowLoopPosition {
    /// The `n`-th evaluation of a `while` condition. The check after the last
    /// body iteration, which reads false, is a check with no body of its
    /// number.
    Check(u64),
    /// The `n`-th run of the loop body.
    Body(u64),
}

/// One enclosing loop of an occurrence, outermost first in a context.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct WorkflowLoopFrame {
    /// The loop's own site.
    pub site: WorkflowSiteRef,
    /// Which entry of that loop this is, unique within the execution: a loop
    /// reentered by an outer iteration gets a new activation.
    pub activation: u64,
    pub position: WorkflowLoopPosition,
}

/// One occurrence of one execution site: the site, which run of that site
/// this is (from 1, counted per site within an execution), and the loop
/// activations that enclosed the occurrence when it began, outermost first.
///
/// Every layer that names an occurrence (durable effect and wait records,
/// trace facts, the overlay and the VM) holds this value.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct WorkflowOccurrence {
    pub site: WorkflowSiteRef,
    pub occurrence: NonZeroU64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loops: Vec<WorkflowLoopFrame>,
}

impl WorkflowOccurrence {
    /// Occurrence `occurrence` of `site`, outside every loop.
    pub fn new(site: WorkflowSiteRef, occurrence: NonZeroU64) -> Self {
        Self {
            site,
            occurrence,
            loops: Vec::new(),
        }
    }

    /// Occurrence `occurrence` of the own statement site of the node spelled
    /// `node`, outside every loop, in a test fixture.
    ///
    /// # Panics
    ///
    /// When `node` is empty or `occurrence` is 0.
    #[doc(hidden)]
    pub fn fixture(node: &str, occurrence: u64) -> Self {
        let Some(occurrence) = NonZeroU64::new(occurrence) else {
            panic!("a fixture occurrence counts from 1");
        };
        Self::new(
            WorkflowSiteRef::node(WorkflowNodeId::fixture(node)),
            occurrence,
        )
    }

    /// The occurrence's identity within its execution: its site and number.
    pub fn key(&self) -> (&WorkflowSiteRef, NonZeroU64) {
        (&self.site, self.occurrence)
    }
}

#[cfg(test)]
#[path = "workflow_site_tests.rs"]
mod tests;
