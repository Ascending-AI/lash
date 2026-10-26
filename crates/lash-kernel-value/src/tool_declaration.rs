//! Declared controls of an operation call.
use serde::{Deserialize, Serialize};

/// A way a tool's call may end its caller's turn.
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
pub enum TurnControlKind {
    /// The turn ends with the value the call carries.
    Finish,
    /// The turn ends by switching to a fresh agent frame.
    SwitchAgentFrame,
}

impl TurnControlKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Finish => "finish",
            Self::SwitchAgentFrame => "switch_agent_frame",
        }
    }
}

/// What a tool that may end its caller's turn with a value declares: the
/// type of that value. Settlement validates the Finish a call emits against
/// it, so the turn's final value has the type of the tool it ended on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FinishDeclaration {
    /// The schema the value of every Finish the tool emits must match.
    pub value_schema: crate::JsonSchema,
}

/// The turn-ending controls a tool declares its result may be, keyed by
/// kind: each kind at most once, and a Finish with the one schema its value
/// must match. A tool that declares any has no output: a control is its
/// call's whole result.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TurnControls {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finish: Option<FinishDeclaration>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    switch_agent_frame: bool,
}

impl TurnControls {
    /// No control: an ordinary tool.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// A Finish whose value must match `value_schema`.
    #[must_use]
    pub fn finish(value_schema: crate::JsonSchema) -> Self {
        Self::none().with_finish(value_schema)
    }

    #[must_use]
    pub fn switch_agent_frame() -> Self {
        Self::none().with_switch_agent_frame()
    }

    /// These controls, also declaring a Finish whose value must match
    /// `value_schema`; it replaces a Finish already declared.
    #[must_use]
    pub fn with_finish(mut self, value_schema: crate::JsonSchema) -> Self {
        self.finish = Some(FinishDeclaration { value_schema });
        self
    }

    /// These controls, also declaring a switch of the agent frame.
    #[must_use]
    pub fn with_switch_agent_frame(mut self) -> Self {
        self.switch_agent_frame = true;
        self
    }

    pub fn contains(&self, kind: TurnControlKind) -> bool {
        match kind {
            TurnControlKind::Finish => self.finish.is_some(),
            TurnControlKind::SwitchAgentFrame => self.switch_agent_frame,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.finish.is_none() && !self.switch_agent_frame
    }

    /// The declared kinds, in vocabulary order.
    pub fn iter(&self) -> impl Iterator<Item = TurnControlKind> + '_ {
        [TurnControlKind::Finish, TurnControlKind::SwitchAgentFrame]
            .into_iter()
            .filter(|kind| self.contains(*kind))
    }

    /// The declared Finish, with the schema its value must match.
    pub fn finish_declaration(&self) -> Option<&FinishDeclaration> {
        self.finish.as_ref()
    }
}
