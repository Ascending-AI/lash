use serde::{Deserialize, Serialize};

/// Closed vocabulary of executable workflow sites.
///
/// This describes the site, not its current observation. A workflow node may
/// expose more than one site kind. Declaration order is the canonical order:
/// a node's execution sites sort by it, so reordering the variants changes
/// the serialized workflow graph and needs a graph schema bump.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionNodeKind {
    ResourceOperation,
    Sleep,
    Wait,
    Terminal,
    ProcessEvent,
    Branch,
    Loop,
    Call,
    Step,
}

impl ExecutionNodeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ResourceOperation => "resource_operation",
            Self::Sleep => "sleep",
            Self::Wait => "wait",
            Self::Terminal => "terminal",
            Self::ProcessEvent => "process_event",
            Self::Branch => "branch",
            Self::Loop => "loop",
            Self::Call => "call",
            Self::Step => "step",
        }
    }
}

impl std::fmt::Display for ExecutionNodeKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<ExecutionNodeKind> for String {
    fn from(kind: ExecutionNodeKind) -> Self {
        kind.as_str().to_owned()
    }
}

impl std::str::FromStr for ExecutionNodeKind {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "resource_operation" => Ok(Self::ResourceOperation),
            "sleep" => Ok(Self::Sleep),
            "wait" => Ok(Self::Wait),
            "terminal" => Ok(Self::Terminal),
            "process_event" => Ok(Self::ProcessEvent),
            "branch" => Ok(Self::Branch),
            "loop" => Ok(Self::Loop),
            "call" => Ok(Self::Call),
            "step" => Ok(Self::Step),
            _ => Err("unknown execution node kind"),
        }
    }
}
