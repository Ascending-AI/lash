use super::GraphRenderError;

impl GraphRenderError {
    /// Stable machine-readable classification for this render failure.
    ///
    /// The closed code set is `unsupported_schema_version`,
    /// `duplicate_node_id`, `unknown_node_reference`,
    /// `invalid_node_payload`, `invalid_expression`,
    /// `invalid_assignment_target`, `invalid_opaque_source`,
    /// `duplicate_process_name`, `process_origin_mismatch`,
    /// `canonical_source`, and `rendered_source_invalid`.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedSchemaVersion(_) => "unsupported_schema_version",
            Self::DuplicateNodeId { .. } => "duplicate_node_id",
            Self::UnknownNodeReference { .. } => "unknown_node_reference",
            Self::InvalidNodePayload { .. } => "invalid_node_payload",
            Self::InvalidExpression { .. } => "invalid_expression",
            Self::InvalidAssignmentTarget { .. } => "invalid_assignment_target",
            Self::InvalidOpaqueSource { .. } => "invalid_opaque_source",
            Self::DuplicateProcessName { .. } => "duplicate_process_name",
            Self::ProcessOriginMismatch { .. } => "process_origin_mismatch",
            Self::CanonicalSource(_) => "canonical_source",
            Self::RenderedSourceInvalid { .. } => "rendered_source_invalid",
        }
    }

    /// The node named by this failure, when the failure is node-addressable.
    pub fn node_id(&self) -> Option<&str> {
        match self {
            Self::DuplicateNodeId { id } => Some(id),
            Self::UnknownNodeReference { node_id, .. }
            | Self::InvalidNodePayload { node_id, .. }
            | Self::InvalidExpression { node_id, .. }
            | Self::InvalidAssignmentTarget { node_id, .. }
            | Self::InvalidOpaqueSource { node_id, .. } => Some(node_id),
            Self::UnsupportedSchemaVersion(_)
            | Self::DuplicateProcessName { .. }
            | Self::ProcessOriginMismatch { .. }
            | Self::CanonicalSource(_)
            | Self::RenderedSourceInvalid { .. } => None,
        }
    }

    /// The editable graph field named by this failure, when one is known.
    pub fn field(&self) -> Option<&str> {
        match self {
            Self::InvalidExpression { field, .. } => Some(field.as_str()),
            Self::InvalidAssignmentTarget { field, .. } => Some(field),
            Self::UnsupportedSchemaVersion(_)
            | Self::DuplicateNodeId { .. }
            | Self::UnknownNodeReference { .. }
            | Self::InvalidNodePayload { .. }
            | Self::InvalidOpaqueSource { .. }
            | Self::DuplicateProcessName { .. }
            | Self::ProcessOriginMismatch { .. }
            | Self::CanonicalSource(_)
            | Self::RenderedSourceInvalid { .. } => None,
        }
    }
}
