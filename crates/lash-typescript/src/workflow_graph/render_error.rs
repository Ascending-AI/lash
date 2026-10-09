use super::GraphRenderError;

impl GraphRenderError {
    /// Stable machine-readable classification for this render failure.
    ///
    /// The lens's own codes are `unsupported_schema_version`,
    /// `invalid_program`, `invalid_expression`, `invalid_assignment_target`
    /// and `canonical_source`; a refusal of the document itself carries the
    /// code of its [`lash_vm::WorkflowGraphError`].
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedSchemaVersion(_) => "unsupported_schema_version",
            Self::Document(error) => error.code(),
            Self::InvalidProgram(_) => "invalid_program",
            Self::InvalidExpression { .. } => "invalid_expression",
            Self::InvalidAssignmentTarget { .. } => "invalid_assignment_target",
            Self::CanonicalSource(_) => "canonical_source",
        }
    }

    /// The node named by this failure, when the failure is node-addressable.
    pub fn node_id(&self) -> Option<&str> {
        match self {
            Self::Document(error) => error.node_id(),
            Self::InvalidExpression { node_id, .. }
            | Self::InvalidAssignmentTarget { node_id, .. } => Some(node_id),
            Self::UnsupportedSchemaVersion(_)
            | Self::InvalidProgram(_)
            | Self::CanonicalSource(_) => None,
        }
    }

    /// The editable graph field named by this failure, when one is known.
    pub fn field(&self) -> Option<&str> {
        match self {
            Self::InvalidExpression { field, .. } => Some(field.as_str()),
            Self::InvalidAssignmentTarget { field, .. } => Some(field),
            Self::UnsupportedSchemaVersion(_)
            | Self::Document(_)
            | Self::InvalidProgram(_)
            | Self::CanonicalSource(_) => None,
        }
    }
}
