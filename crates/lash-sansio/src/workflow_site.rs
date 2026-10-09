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

/// One step of a [`WorkflowSitePath`].
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSiteSegment {
    /// A typed child slot of the expression reached so far.
    Slot(ExprSlot),
    /// A site the expression reached so far owns beside its own.
    Role(WorkflowSiteRole),
}

/// The typed path from a workflow node's statement to one executable
/// subexpression. The empty path is the statement itself.
///
/// Slot segments walk the statement's typed child slots; a trailing role
/// segment names a synthetic site of the expression they reach.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct WorkflowSitePath(pub Vec<WorkflowSiteSegment>);

impl WorkflowSitePath {
    /// The path through `slots`, from a node's statement.
    pub fn slots(slots: impl IntoIterator<Item = ExprSlot>) -> Self {
        Self(slots.into_iter().map(WorkflowSiteSegment::Slot).collect())
    }

    /// This path with a synthetic `role` site of the expression it reaches.
    #[must_use]
    pub fn role(mut self, role: WorkflowSiteRole) -> Self {
        self.0.push(WorkflowSiteSegment::Role(role));
        self
    }

    /// The slots that reach the site's expression, without its role.
    pub fn expr_slots(&self) -> impl Iterator<Item = ExprSlot> + '_ {
        self.0.iter().filter_map(|segment| match segment {
            WorkflowSiteSegment::Slot(slot) => Some(*slot),
            WorkflowSiteSegment::Role(_) => None,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Display for WorkflowSitePath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for segment in &self.0 {
            match segment {
                WorkflowSiteSegment::Slot(slot) => write!(formatter, "/{slot}")?,
                WorkflowSiteSegment::Role(WorkflowSiteRole::LabeledStep) => {
                    formatter.write_str("#labeled_step")?;
                }
            }
        }
        Ok(())
    }
}

/// The static address of one execution site in a workflow document: the node
/// that owns it and the path to the executable subexpression inside it.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSiteRef {
    pub node_id: String,
    #[serde(default, skip_serializing_if = "WorkflowSitePath::is_empty")]
    pub site_path: WorkflowSitePath,
}

impl WorkflowSiteRef {
    pub fn new(node_id: impl Into<String>, site_path: WorkflowSitePath) -> Self {
        Self {
            node_id: node_id.into(),
            site_path,
        }
    }

    /// The node's own statement site.
    pub fn node(node_id: impl Into<String>) -> Self {
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

/// Where one occurrence of a site ran, beyond its node: the exact site
/// inside the node's statement and the loop activations that enclosed the
/// occurrence when it began, outermost first. The default is a node's own
/// statement site outside every loop.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct WorkflowOccurrenceContext {
    #[serde(default, skip_serializing_if = "WorkflowSitePath::is_empty")]
    pub site_path: WorkflowSitePath,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loops: Vec<WorkflowLoopFrame>,
}

impl WorkflowOccurrenceContext {
    pub fn is_default(&self) -> bool {
        self.site_path.is_empty() && self.loops.is_empty()
    }
}
