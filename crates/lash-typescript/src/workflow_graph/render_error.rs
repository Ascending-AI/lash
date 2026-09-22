use super::GraphRenderError;

impl GraphRenderError {
    /// Stable machine-readable classification for this render failure.
    ///
    /// The closed code set is `unsupported_schema_version`,
    /// `duplicate_node_id`, `unknown_node_reference`,
    /// `invalid_node_payload`, `invalid_expression`,
    /// `invalid_assignment_target`, `invalid_opaque_source`,
    /// `duplicate_process_name`, `canonical_source`, and
    /// `rendered_source_invalid`.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedSchemaVersion { .. } => "unsupported_schema_version",
            Self::DuplicateNodeId { .. } => "duplicate_node_id",
            Self::UnknownNodeReference { .. } => "unknown_node_reference",
            Self::InvalidNodePayload { .. } => "invalid_node_payload",
            Self::InvalidExpression { .. } => "invalid_expression",
            Self::InvalidAssignmentTarget { .. } => "invalid_assignment_target",
            Self::InvalidOpaqueSource { .. } => "invalid_opaque_source",
            Self::DuplicateProcessName { .. } => "duplicate_process_name",
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
            Self::UnsupportedSchemaVersion { .. }
            | Self::DuplicateProcessName { .. }
            | Self::CanonicalSource(_)
            | Self::RenderedSourceInvalid { .. } => None,
        }
    }

    /// The editable graph field named by this failure, when one is known.
    pub fn field(&self) -> Option<&str> {
        match self {
            Self::InvalidExpression { field, .. } => Some(field.as_str()),
            Self::InvalidAssignmentTarget { field, .. } => Some(field),
            Self::UnsupportedSchemaVersion { .. }
            | Self::DuplicateNodeId { .. }
            | Self::UnknownNodeReference { .. }
            | Self::InvalidNodePayload { .. }
            | Self::InvalidOpaqueSource { .. }
            | Self::DuplicateProcessName { .. }
            | Self::CanonicalSource(_)
            | Self::RenderedSourceInvalid { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow_graph::{GraphRenderErrorDiscriminants, TypeScriptSourceError};
    use strum::IntoEnumIterator;

    #[test]
    fn every_error_kind_has_a_literal_code_and_location_oracle() {
        let oracles = [
            (
                GraphRenderError::UnsupportedSchemaVersion {
                    found: 12,
                    expected: 13,
                },
                "unsupported_schema_version",
                None,
                None,
            ),
            (
                GraphRenderError::DuplicateNodeId {
                    id: "node:duplicate".to_string(),
                },
                "duplicate_node_id",
                Some("node:duplicate"),
                None,
            ),
            (
                GraphRenderError::UnknownNodeReference {
                    edge_id: "edge:1".to_string(),
                    endpoint: "target",
                    node_id: "node:missing".to_string(),
                },
                "unknown_node_reference",
                Some("node:missing"),
                None,
            ),
            (
                GraphRenderError::InvalidNodePayload {
                    node_id: "node:payload".to_string(),
                    message: "bad payload".to_string(),
                },
                "invalid_node_payload",
                Some("node:payload"),
                None,
            ),
            (
                GraphRenderError::InvalidExpression {
                    node_id: "node:expression".to_string(),
                    field: "condition".to_string(),
                    message: "bad expression".to_string(),
                },
                "invalid_expression",
                Some("node:expression"),
                Some("condition"),
            ),
            (
                GraphRenderError::InvalidAssignmentTarget {
                    node_id: "node:target".to_string(),
                    field: "target",
                    message: "bad target".to_string(),
                },
                "invalid_assignment_target",
                Some("node:target"),
                Some("target"),
            ),
            (
                GraphRenderError::InvalidOpaqueSource {
                    node_id: "node:opaque".to_string(),
                    message: "bad source".to_string(),
                },
                "invalid_opaque_source",
                Some("node:opaque"),
                None,
            ),
            (
                GraphRenderError::DuplicateProcessName {
                    name: "child".to_string(),
                },
                "duplicate_process_name",
                None,
                None,
            ),
            (
                GraphRenderError::CanonicalSource(TypeScriptSourceError::Unrepresentable {
                    kind: "fixture",
                }),
                "canonical_source",
                None,
                None,
            ),
            (
                GraphRenderError::RenderedSourceInvalid {
                    message: "fixture".to_string(),
                },
                "rendered_source_invalid",
                None,
                None,
            ),
        ];

        let actual_kinds = oracles
            .iter()
            .map(|(error, ..)| GraphRenderErrorDiscriminants::from(error))
            .collect::<std::collections::BTreeSet<_>>();
        let expected_kinds =
            GraphRenderErrorDiscriminants::iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(actual_kinds, expected_kinds);

        for (error, code, node_id, field) in oracles {
            assert_eq!(error.code(), code);
            assert_eq!(error.node_id(), node_id);
            assert_eq!(error.field(), field);
        }
    }
}
